use crate::anonymizer::{CodeAnonymizer, CodeAnonymizerOptions};
use nova_config::AiPrivacyConfig;

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
}
