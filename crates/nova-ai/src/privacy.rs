use crate::anonymizer::{CodeAnonymizer, CodeAnonymizerOptions};
use nova_config::AiPrivacyConfig;
use once_cell::sync::Lazy;
use regex::Regex;

/// Privacy controls that apply when building prompts for cloud models.
#[derive(Debug, Clone)]
pub struct PrivacyMode {
    /// Replace identifiers (class/method/variable names) with stable placeholders.
    pub anonymize_identifiers: bool,

    /// Redact suspicious literals (API keys, tokens, long IDs, etc).
    pub redaction: RedactionConfig,

    /// Whether the builder is allowed to include file system paths.
    pub include_file_paths: bool,
}

impl Default for PrivacyMode {
    fn default() -> Self {
        Self {
            anonymize_identifiers: false,
            redaction: RedactionConfig::default(),
            include_file_paths: false,
        }
    }
}

impl PrivacyMode {
    /// Build a [`PrivacyMode`] from the user-facing [`AiPrivacyConfig`].
    ///
    /// This is primarily used by prompt builders (e.g. LSP AI actions) that need
    /// to apply the same privacy defaults as the provider client.
    pub fn from_ai_privacy_config(config: &AiPrivacyConfig) -> Self {
        Self {
            anonymize_identifiers: config.effective_anonymize_identifiers(),
            redaction: RedactionConfig {
                redact_string_literals: config.effective_redact_sensitive_strings(),
                redact_numeric_literals: config.effective_redact_numeric_literals(),
                redact_comments: config.effective_strip_or_redact_comments(),
            },
            // Paths are excluded by default (see docs/13-ai-augmentation.md).
            // Call sites must opt in explicitly (via config or legacy env vars).
            include_file_paths: config.include_file_paths,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RedactionConfig {
    pub redact_string_literals: bool,
    pub redact_numeric_literals: bool,
    pub redact_comments: bool,
}

impl Default for RedactionConfig {
    fn default() -> Self {
        Self {
            redact_string_literals: true,
            redact_numeric_literals: true,
            redact_comments: true,
        }
    }
}

pub fn redact_suspicious_literals(code: &str, cfg: &RedactionConfig) -> String {
    let mut anonymizer = CodeAnonymizer::new(CodeAnonymizerOptions {
        anonymize_identifiers: false,
        redact_sensitive_strings: cfg.redact_string_literals,
        redact_numeric_literals: cfg.redact_numeric_literals,
        strip_or_redact_comments: cfg.redact_comments,
    });
    anonymizer.anonymize(code)
}

pub(crate) fn redact_file_paths(text: &str) -> String {
    use std::borrow::Cow;

    // `file://` URIs (both Unix and Windows forms).
    //
    // We keep this regex intentionally permissive and redact the full URI token to avoid leaking
    // sensitive path metadata via common error formats (e.g. stack traces that include
    // `file:///...` locations).
    //
    // Examples:
    // - file:///home/alice/project/Secret.java
    // - file:///C:/Users/Alice/Secret.java
    // - file://localhost/home/alice/project/Secret.java
    //
    // We stop at common delimiters so surrounding punctuation is preserved (e.g. `(... )`).
    static FILE_URI_RE: Lazy<Regex> = Lazy::new(|| {
        // Note: Java commonly emits `file:/...` for absolute file URIs (single slash), while other
        // tooling emits `file:///...`. We treat any `file:` token with an immediate, non-delimited
        // payload as a potential path leak.
        Regex::new(r#"(?mi)(?P<path>\bfile:[^\r\n"'<>)\]}]+)"#).expect("valid file uri regex")
    });

    // UNC paths / network shares (e.g. `\\server\share\path\file.txt`), including the escaped form
    // that appears in serialized strings (`\\\\server\\\\share\\\\path`).
    static WINDOWS_UNC_PATH_RE: Lazy<Regex> = Lazy::new(|| {
        // Require 2+ characters for the server/share segments to avoid accidentally matching common
        // escape sequences in code (e.g. `\\n\\t`).
        Regex::new(r"(?m)(?P<path>\\{2,}[A-Za-z0-9._$-]{2,}\\+[A-Za-z0-9._$-]{2,}(?:\\+[A-Za-z0-9._$()-]+(?: [A-Za-z0-9._$()-]+)*)*)")
            .expect("valid windows UNC path regex")
    });

    // Windows "device" path prefixes like `\\?\C:\...`, `\\?\UNC\server\share\...`, or `\\.\pipe\...`.
    //
    // These show up in Windows APIs, Java/Rust stack traces, and some build tooling.
    static WINDOWS_DEVICE_PATH_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r#"(?m)(?P<path>\\{2,}[?.]\\+[^\r\n"'<>)\]}]+)"#)
            .expect("valid windows device path regex")
    });
    // Absolute *nix paths.
    static UNIX_PATH_RE: Lazy<Regex> = Lazy::new(|| {
        // Allow spaces inside directory segments (common in macOS/Windows-y projects), but keep the
        // final path segment space-free so we don't greedily consume non-path prose following the
        // path token.
        Regex::new(
            r"(?m)(?P<path>/(?:[A-Za-z0-9._()\\-]+(?: [A-Za-z0-9._()\\-]+)*/)+[A-Za-z0-9._()\\-]+)",
        )
            .expect("valid unix path regex")
    });
    // Basic Windows drive paths (e.g. `C:\Users\alice\file.txt`).
    //
    // This intentionally matches one or more backslashes so we redact both the raw form (single
    // backslashes) and the escaped form that often appears in serialized/quoted strings (double
    // backslashes).
    static WINDOWS_PATH_RE: Lazy<Regex> = Lazy::new(|| {
        // Also match the forward-slash form (`C:/Users/alice/file.txt`) which is common in some
        // toolchains and cross-platform logs.
        Regex::new(
            r"(?m)(?P<path>[A-Za-z]:[\\/]+(?:[A-Za-z0-9._$()\\-]+(?: [A-Za-z0-9._$()\\-]+)*[\\/]+)*[A-Za-z0-9._$()\\-]+)",
        )
            .expect("valid windows path regex")
    });

    // This function is on the prompt-building hot path. Use `Cow` so we only allocate when a
    // replacement actually occurs, rather than allocating once per regex stage.
    let mut out = Cow::Borrowed(text);

    let replaced = FILE_URI_RE.replace_all(out.as_ref(), "[PATH]");
    if let Cow::Owned(s) = replaced {
        out = Cow::Owned(s);
    }

    let replaced = WINDOWS_DEVICE_PATH_RE.replace_all(out.as_ref(), "[PATH]");
    if let Cow::Owned(s) = replaced {
        out = Cow::Owned(s);
    }

    let replaced = WINDOWS_UNC_PATH_RE.replace_all(out.as_ref(), "[PATH]");
    if let Cow::Owned(s) = replaced {
        out = Cow::Owned(s);
    }

    let replaced = UNIX_PATH_RE.replace_all(out.as_ref(), "[PATH]");
    if let Cow::Owned(s) = replaced {
        out = Cow::Owned(s);
    }

    let replaced = WINDOWS_PATH_RE.replace_all(out.as_ref(), "[PATH]");
    if let Cow::Owned(s) = replaced {
        out = Cow::Owned(s);
    }

    out.into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_suspicious_string_literals() {
        let code = r#"String apiKey = "sk-verysecretstringthatislong";"#;
        let redacted = redact_suspicious_literals(code, &RedactionConfig::default());
        assert!(redacted.contains("\"[REDACTED]\""));
        assert!(!redacted.contains("sk-verysecret"));
    }

    #[test]
    fn redacts_sensitive_comments() {
        let code = r#"// token: sk-verysecretstringthatislong
 class Foo {}"#;
        let redacted = redact_suspicious_literals(code, &RedactionConfig::default());
        assert!(redacted.contains("// [REDACTED]"));
        assert!(!redacted.contains("sk-verysecret"));
    }

    #[test]
    fn privacy_mode_from_config_respects_local_only_defaults() {
        let cfg = AiPrivacyConfig {
            local_only: true,
            ..AiPrivacyConfig::default()
        };
        let mode = PrivacyMode::from_ai_privacy_config(&cfg);
        assert!(!mode.anonymize_identifiers);
        assert!(!mode.redaction.redact_string_literals);
        assert!(!mode.redaction.redact_numeric_literals);
        assert!(!mode.redaction.redact_comments);
        assert!(!mode.include_file_paths);
    }

    #[test]
    fn privacy_mode_from_config_respects_cloud_defaults() {
        let cfg = AiPrivacyConfig {
            local_only: false,
            ..AiPrivacyConfig::default()
        };
        let mode = PrivacyMode::from_ai_privacy_config(&cfg);
        assert!(mode.anonymize_identifiers);
        assert!(mode.redaction.redact_string_literals);
        assert!(mode.redaction.redact_numeric_literals);
        assert!(mode.redaction.redact_comments);
        assert!(!mode.include_file_paths);
    }

    #[test]
    fn privacy_mode_from_config_excludes_paths_by_default() {
        let cfg = AiPrivacyConfig::default();
        let mode = PrivacyMode::from_ai_privacy_config(&cfg);
        assert!(!mode.include_file_paths);
    }

    #[test]
    fn privacy_mode_from_config_includes_paths_when_opted_in() {
        let cfg = AiPrivacyConfig {
            include_file_paths: true,
            ..AiPrivacyConfig::default()
        };
        let mode = PrivacyMode::from_ai_privacy_config(&cfg);
        assert!(mode.include_file_paths);
    }

    #[test]
    fn redact_file_paths_rewrites_unix_absolute_paths() {
        let prompt = r#"String p = "/home/alice/project/secret.txt";"#;
        let out = redact_file_paths(prompt);
        assert!(out.contains("[PATH]"), "{out}");
        assert!(
            !out.contains("/home/alice/project/secret.txt"),
            "{out}"
        );
    }

    #[test]
    fn redact_file_paths_rewrites_windows_absolute_paths() {
        let prompt = r#"String p = "C:\\Users\\alice\\secret.txt";"#;
        let out = redact_file_paths(prompt);
        assert!(out.contains("[PATH]"), "{out}");
        assert!(
            !out.contains(r"C:\\Users\\alice\\secret.txt"),
            "{out}"
        );
    }

    #[test]
    fn redact_file_paths_rewrites_windows_absolute_paths_with_single_backslashes() {
        let prompt = r#"log("opening C:\Users\alice\secret.txt")"#;
        let out = redact_file_paths(prompt);
        assert!(out.contains("[PATH]"), "{out}");
        assert!(!out.contains(r"C:\Users\alice\secret.txt"), "{out}");
    }

    #[test]
    fn redact_file_paths_rewrites_windows_absolute_paths_with_forward_slashes() {
        let prompt = r#"log("opening C:/Users/alice/secret.txt")"#;
        let out = redact_file_paths(prompt);
        assert!(out.contains("[PATH]"), "{out}");
        assert!(!out.contains("C:/Users/alice/secret.txt"), "{out}");
    }

    #[test]
    fn redact_file_paths_rewrites_unix_file_uris() {
        let prompt = r#"opening file:///home/alice/project/Secret.java"#;
        let out = redact_file_paths(prompt);
        assert!(out.contains("[PATH]"), "{out}");
        assert!(!out.to_lowercase().contains("file:"), "{out}");
        assert!(!out.contains("file:///home/alice/project/Secret.java"), "{out}");
        assert!(!out.contains("/home/alice/project/Secret.java"), "{out}");
    }

    #[test]
    fn redact_file_paths_rewrites_windows_file_uris() {
        let prompt = r#"opening file:///C:/Users/Alice/Secret.java"#;
        let out = redact_file_paths(prompt);
        assert!(out.contains("[PATH]"), "{out}");
        assert!(!out.to_lowercase().contains("file:"), "{out}");
        assert!(
            !out.contains("file:///C:/Users/Alice/Secret.java"),
            "{out}"
        );
        assert!(!out.contains("C:/Users/Alice/Secret.java"), "{out}");
    }

    #[test]
    fn redact_file_paths_rewrites_java_style_file_uris() {
        let prompt = r#"opening file:/home/alice/project/Secret.java and file:/C:/Users/Alice/Secret.java"#;
        let out = redact_file_paths(prompt);
        assert!(out.contains("[PATH]"), "{out}");
        assert!(!out.to_lowercase().contains("file:"), "{out}");
        assert!(!out.contains("/home/alice/project/Secret.java"), "{out}");
        assert!(!out.contains("C:/Users/Alice/Secret.java"), "{out}");
    }

    #[test]
    fn redact_file_paths_rewrites_unc_paths() {
        let prompt = r#"opening \\server123\share456\Users\alice\secret.txt"#;
        let out = redact_file_paths(prompt);
        assert!(out.contains("[PATH]"), "{out}");
        assert!(
            !out.contains(r"\\server123\share456\Users\alice\secret.txt"),
            "{out}"
        );
        assert!(!out.contains("server123"), "{out}");
        assert!(!out.contains("share456"), "{out}");
        assert!(!out.contains("secret.txt"), "{out}");
    }

    #[test]
    fn redact_file_paths_rewrites_windows_device_paths() {
        let prompt = r#"opening \\?\UNC\server123\share456\Users\alice\secret.txt"#;
        let out = redact_file_paths(prompt);
        assert!(out.contains("[PATH]"), "{out}");
        assert!(!out.contains(r"\\?\UNC\server123\share456"), "{out}");
        assert!(!out.contains("server123"), "{out}");
        assert!(!out.contains("share456"), "{out}");
    }

    #[test]
    fn redact_file_paths_rewrites_windows_paths_with_spaces_and_parentheses() {
        let prompt = r#"opening C:\Program Files (x86)\Acme\secret.txt"#;
        let out = redact_file_paths(prompt);
        assert!(out.contains("[PATH]"), "{out}");
        assert!(!out.contains(r"C:\Program Files (x86)\Acme\secret.txt"), "{out}");
        assert!(!out.contains("Program Files"), "{out}");
        assert!(!out.contains("Acme"), "{out}");
        assert!(!out.contains("secret.txt"), "{out}");
    }

    #[test]
    fn redact_file_paths_rewrites_unix_paths_with_spaces_in_segments() {
        let prompt = r#"opening /Users/alice/My Project/secret.txt"#;
        let out = redact_file_paths(prompt);
        assert!(out.contains("[PATH]"), "{out}");
        assert!(!out.contains("/Users/alice/My Project/secret.txt"), "{out}");
        assert!(!out.contains("My Project"), "{out}");
        assert!(!out.contains("secret.txt"), "{out}");
    }

    #[test]
    fn redact_file_paths_rewrites_windows_device_paths_with_spaces() {
        let prompt = r#"opening \\?\C:\Program Files\Acme\secret.txt"#;
        let out = redact_file_paths(prompt);
        assert!(out.contains("[PATH]"), "{out}");
        assert!(!out.contains(r"\\?\C:\Program Files\Acme\secret.txt"), "{out}");
        assert!(!out.contains("Program Files"), "{out}");
        assert!(!out.contains("Acme"), "{out}");
        assert!(!out.contains("secret.txt"), "{out}");
    }
}
