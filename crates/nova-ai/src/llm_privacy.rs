use crate::anonymizer::{CodeAnonymizer, CodeAnonymizerOptions};
use crate::{types::CodeSnippet, AiError};
use globset::{Glob, GlobSet, GlobSetBuilder};
use nova_config::AiPrivacyConfig;
use regex::Regex;
use std::path::{Component, Path, PathBuf};

/// Matches file paths against [`AiPrivacyConfig::excluded_paths`].
///
/// This is exposed as a lightweight, provider-independent API so callers (e.g. LSP/IDE) can
/// determine whether a file is excluded *before* attempting to initialize an LLM provider.
///
/// Matching semantics:
/// - Patterns are compiled using `globset` (same as [`PrivacyFilter`]).
/// - If the candidate path is absolute and the initial match fails, we also try matching against
///   each suffix of the path components. This allows relative patterns like `secret/**` to match
///   absolute filesystem paths such as `/home/user/project/secret/file.txt`.
#[derive(Debug)]
pub struct ExcludedPathMatcher {
    set: GlobSet,
}

impl ExcludedPathMatcher {
    pub fn from_config(config: &AiPrivacyConfig) -> Result<Self, AiError> {
        Self::new(&config.excluded_paths)
    }

    pub fn new(patterns: &[String]) -> Result<Self, AiError> {
        let mut builder = GlobSetBuilder::new();
        for pattern in patterns {
            let glob = Glob::new(pattern).map_err(|err| {
                AiError::InvalidConfig(format!("invalid excluded_paths glob {pattern:?}: {err}"))
            })?;
            builder.add(glob);
        }

        let set = builder.build().map_err(|err| {
            AiError::InvalidConfig(format!("failed to build excluded_paths globset: {err}"))
        })?;

        Ok(Self { set })
    }

    pub fn is_match(&self, path: &Path) -> bool {
        if self.set.is_match(path) {
            return true;
        }

        // Best-effort lexical normalization for paths that include `..` segments. This matters in
        // particular for relative paths like `public/../secret/file`, which should still match an
        // excluded pattern like `secret/**`.
        //
        // Note: we only apply this normalization as a fallback (when the raw match fails). This
        // mirrors the absolute-path suffix logic below and errs on the side of over-excluding
        // rather than risking a bypass.
        #[derive(Debug, Clone, Copy)]
        enum NormalizedComponent<'a> {
            Parent,
            Normal(&'a std::ffi::OsStr),
        }

        let is_absolute = path.is_absolute();
        let mut normalized_components = Vec::<NormalizedComponent<'_>>::new();
        for component in path.components() {
            match component {
                Component::Prefix(_) | Component::RootDir | Component::CurDir => {}
                Component::ParentDir => match normalized_components.last() {
                    Some(NormalizedComponent::Normal(_)) => {
                        normalized_components.pop();
                    }
                    Some(NormalizedComponent::Parent) | None => {
                        // Absolute paths can't meaningfully traverse above root; for relative paths,
                        // preserve leading `..` segments since they may be semantically relevant.
                        if !is_absolute {
                            normalized_components.push(NormalizedComponent::Parent);
                        }
                    }
                },
                Component::Normal(segment) => {
                    normalized_components.push(NormalizedComponent::Normal(segment))
                }
            }
        }

        if !normalized_components.is_empty() {
            let mut normalized = PathBuf::new();
            for component in &normalized_components {
                match component {
                    NormalizedComponent::Parent => normalized.push(".."),
                    NormalizedComponent::Normal(segment) => normalized.push(segment),
                }
            }

            if self.set.is_match(&normalized) {
                return true;
            }
        }

        // `globset` patterns are typically configured as paths relative to some workspace root
        // (e.g. "secret/**"), while callers like LSP generally deal in absolute filesystem paths.
        //
        // Since we don't have access to the caller's notion of "workspace root" here, we treat
        // absolute paths as match candidates for *any* suffix of the path components.
        if !path.is_absolute() {
            return false;
        }

        // For absolute paths, the normalization above will not include `..` segments, so we can
        // safely treat the normalized components as plain path segments.
        let segments: Vec<&std::ffi::OsStr> = normalized_components
            .iter()
            .filter_map(|component| match component {
                NormalizedComponent::Normal(segment) => Some(*segment),
                NormalizedComponent::Parent => None,
            })
            .collect();

        for start in 0..segments.len() {
            let mut suffix = PathBuf::new();
            for segment in &segments[start..] {
                suffix.push(*segment);
            }
            if self.set.is_match(&suffix) {
                return true;
            }
        }

        false
    }
}

/// Privacy filtering for LLM backends configured via `nova-config`.
///
/// This sits alongside (and intentionally separate from) `nova_ai::privacy`,
/// which focuses on prompt-building and token redaction/anonymization heuristics.
pub struct PrivacyFilter {
    excluded_paths: ExcludedPathMatcher,
    redact_patterns: Vec<Regex>,
    anonymizer_options: CodeAnonymizerOptions,
    use_anonymizer: bool,
}

/// Request-scoped state for privacy sanitization.
///
/// This is intentionally lightweight and meant to be created per request. It
/// ensures anonymization is stable across multiple snippets/messages within the
/// same request while avoiding cross-request identifier reuse.
#[derive(Debug)]
pub(crate) struct SanitizationSession {
    anonymizer: CodeAnonymizer,
}

impl SanitizationSession {
    fn new(options: CodeAnonymizerOptions) -> Self {
        Self {
            anonymizer: CodeAnonymizer::new(options),
        }
    }
}

impl PrivacyFilter {
    pub fn new(config: &AiPrivacyConfig) -> Result<Self, AiError> {
        let excluded_paths = ExcludedPathMatcher::from_config(config)?;

        let mut redact_patterns = Vec::new();
        for pattern in &config.redact_patterns {
            let re = Regex::new(pattern).map_err(|err| {
                AiError::InvalidConfig(format!("invalid redact_patterns regex {pattern:?}: {err}"))
            })?;
            redact_patterns.push(re);
        }

        let anonymizer_options = CodeAnonymizerOptions {
            anonymize_identifiers: config.effective_anonymize_identifiers(),
            redact_sensitive_strings: config.effective_redact_sensitive_strings(),
            redact_numeric_literals: config.effective_redact_numeric_literals(),
            strip_or_redact_comments: config.effective_strip_or_redact_comments(),
        };
        let use_anonymizer = anonymizer_options.anonymize_identifiers
            || anonymizer_options.redact_sensitive_strings
            || anonymizer_options.redact_numeric_literals
            || anonymizer_options.strip_or_redact_comments;

        Ok(Self {
            excluded_paths,
            redact_patterns,
            anonymizer_options,
            use_anonymizer,
        })
    }

    pub(crate) fn new_session(&self) -> SanitizationSession {
        SanitizationSession::new(self.anonymizer_options)
    }

    pub fn is_excluded(&self, path: &Path) -> bool {
        self.excluded_paths.is_match(path)
    }

    /// Apply redaction patterns to arbitrary prompt text.
    pub fn sanitize_prompt_text(&self, session: &mut SanitizationSession, text: &str) -> String {
        sanitize_markdown_fenced_code_blocks(
            text,
            |block| {
                let sanitized = if self.use_anonymizer {
                    session.anonymizer.anonymize(block)
                } else {
                    block.to_string()
                };
                self.apply_redaction(&sanitized)
            },
            |plain| self.apply_redaction(plain),
        )
    }

    /// Apply redaction and (optionally) anonymization to code before sending it to an LLM.
    pub fn sanitize_code_text(&self, session: &mut SanitizationSession, code: &str) -> String {
        let sanitized = if self.use_anonymizer {
            session.anonymizer.anonymize(code)
        } else {
            code.to_string()
        };
        self.apply_redaction(&sanitized)
    }

    pub fn sanitize_snippet(
        &self,
        session: &mut SanitizationSession,
        snippet: &CodeSnippet,
    ) -> Option<String> {
        if let Some(path) = snippet.path.as_deref() {
            if self.is_excluded(path) {
                return None;
            }
        }

        Some(self.sanitize_code_text(session, &snippet.content))
    }

    fn apply_redaction(&self, text: &str) -> String {
        let mut output = text.to_string();
        for re in &self.redact_patterns {
            output = re.replace_all(&output, "[REDACTED]").into_owned();
        }
        output
    }
}

fn sanitize_markdown_fenced_code_blocks<FCode, FPlain>(
    text: &str,
    mut sanitize_code: FCode,
    mut sanitize_plain: FPlain,
) -> String
where
    FCode: FnMut(&str) -> String,
    FPlain: FnMut(&str) -> String,
{
    // We implement a tiny parser instead of pulling in a Markdown crate. The
    // rules are deliberately simple:
    // - A fence starts at "```" and ends at the next "```".
    // - The first line after the opening fence is treated as the info string
    //   (language tag) and is preserved verbatim.
    let mut out = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(start) = rest.find("```") {
        let (prefix, after_start) = rest.split_at(start);
        out.push_str(&sanitize_plain(prefix));

        // Skip the opening fence.
        let mut after_start = &after_start[3..];

        // Preserve the info string line (up to and including newline if present).
        if let Some(info_end) = after_start.find('\n') {
            let (info_line, after_info) = after_start.split_at(info_end + 1);
            out.push_str("```");
            out.push_str(info_line);
            after_start = after_info;
        } else {
            // No newline => fence with no body.
            out.push_str("```");
            out.push_str(after_start);
            return out;
        }

        // Find closing fence.
        if let Some(end) = after_start.find("```") {
            let (code_body, after_code) = after_start.split_at(end);
            out.push_str(&sanitize_code(code_body));
            out.push_str("```");
            rest = &after_code[3..];
        } else {
            // Unterminated fence: treat remainder as plain text.
            out.push_str(&sanitize_plain(after_start));
            return out;
        }
    }

    out.push_str(&sanitize_plain(rest));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fenced_code_blocks_are_anonymized_with_stable_mapping() {
        let cfg = AiPrivacyConfig {
            local_only: false,
            anonymize_identifiers: Some(true),
            ..AiPrivacyConfig::default()
        };
        let filter = PrivacyFilter::new(&cfg).expect("filter");
        let mut session = filter.new_session();

        let msg1 = "First:\n```java\nclass Foo { Foo foo; }\n```\n";
        let msg2 = "Second:\n```java\nFoo other = foo;\n```\n";

        let out1 = filter.sanitize_prompt_text(&mut session, msg1);
        let out2 = filter.sanitize_prompt_text(&mut session, msg2);

        // The same identifier should map to the same placeholder across calls.
        assert!(out1.contains("class id_0"), "{out1}");
        assert!(out1.contains("id_1"), "{out1}");
        assert!(out2.contains("id_0"), "{out2}");
        assert!(out2.contains("id_1"), "{out2}");
        // Ensure we didn't anonymize the language tag / fence markers.
        assert!(out1.contains("```java"));
        assert!(out1.contains("```"));
    }

    #[test]
    fn cloud_redacts_sensitive_strings_by_default_even_when_identifier_anonymization_disabled() {
        let cfg = AiPrivacyConfig {
            local_only: false,
            anonymize_identifiers: Some(false),
            ..AiPrivacyConfig::default()
        };
        let filter = PrivacyFilter::new(&cfg).expect("filter");
        let mut session = filter.new_session();

        let code = r#"class SecretService { String apiKey = "sk-verysecretstringthatislong"; }"#;
        let out = filter.sanitize_code_text(&mut session, code);

        assert!(out.contains("\"[REDACTED]\""), "{out}");
        assert!(out.contains("SecretService"), "{out}");
        assert!(!out.contains("id_0"), "{out}");
        assert!(!out.contains("sk-verysecret"), "{out}");
    }

    #[test]
    fn preserves_code_edit_range_markers_when_comment_stripping_enabled() {
        // Cloud mode defaults to comment stripping and literal redaction even when identifier
        // anonymization is disabled (required for cloud code edits). The synthetic range markers
        // used by Nova's code-edit prompts must remain intact so the model can locate the edit
        // region.
        let cfg = AiPrivacyConfig {
            local_only: false,
            anonymize_identifiers: Some(false),
            ..AiPrivacyConfig::default()
        };
        let filter = PrivacyFilter::new(&cfg).expect("filter");
        let mut session = filter.new_session();

        let prompt = r#"Edit the marked range:
```java
/*__NOVA_AI_RANGE_START__*/
class Foo {
  /* secret */ String apiKey = "sk-verysecretstringthatislong";
}
/*__NOVA_AI_RANGE_END__*/
```
"#;

        let out = filter.sanitize_prompt_text(&mut session, prompt);
        assert!(out.contains("/*__NOVA_AI_RANGE_START__*/"), "{out}");
        assert!(out.contains("/*__NOVA_AI_RANGE_END__*/"), "{out}");
        assert!(out.contains("/* [REDACTED] */"), "{out}");
        assert!(out.contains("\"[REDACTED]\""), "{out}");
        assert!(!out.contains("secret"), "{out}");
        assert!(!out.contains("sk-verysecret"), "{out}");
    }

    #[test]
    fn nova_ai_prompts_do_not_leak_identifiers_outside_code_fences_in_cloud_mode() {
        let cfg = AiPrivacyConfig {
            local_only: false,
            anonymize_identifiers: Some(true),
            ..AiPrivacyConfig::default()
        };
        let filter = PrivacyFilter::new(&cfg).expect("filter");
        let mut session = filter.new_session();

        let secret = "SecretService";
        let context = "## Focal code\nclass Foo {}\n";

        let explain_prompt =
            crate::actions::explain_error_prompt(&format!("cannot find symbol: {secret}"), context);
        assert!(
            explain_prompt.contains(secret),
            "prompt should include raw input"
        );
        let explain_out = filter.sanitize_prompt_text(&mut session, &explain_prompt);
        assert!(
            !explain_out.contains(secret),
            "sanitized prompt should not leak identifiers: {explain_out}"
        );

        let method_sig = format!("public {secret} make({secret} svc)");
        let method_prompt = crate::actions::generate_method_body_prompt(&method_sig, context);
        assert!(
            method_prompt.contains(secret),
            "prompt should include raw input"
        );
        let method_out = filter.sanitize_prompt_text(&mut session, &method_prompt);
        assert!(
            !method_out.contains(secret),
            "sanitized prompt should not leak identifiers: {method_out}"
        );

        let target = format!("Generate tests for {secret}#run");
        let tests_prompt = crate::actions::generate_tests_prompt(&target, context);
        assert!(
            tests_prompt.contains(secret),
            "prompt should include raw input"
        );
        let tests_out = filter.sanitize_prompt_text(&mut session, &tests_prompt);
        assert!(
            !tests_out.contains(secret),
            "sanitized prompt should not leak identifiers: {tests_out}"
        );
    }

    #[test]
    fn explain_error_prompt_escapes_fence_markers_to_keep_sanitization_intact() {
        let cfg = AiPrivacyConfig {
            local_only: false,
            anonymize_identifiers: Some(true),
            ..AiPrivacyConfig::default()
        };
        let filter = PrivacyFilter::new(&cfg).expect("filter");
        let mut session = filter.new_session();

        let secret = "SecretService";
        let diagnostic_message = format!("cannot find symbol: ```{secret}```");
        let prompt = crate::actions::explain_error_prompt(&diagnostic_message, "");
        assert!(
            prompt.contains(secret),
            "prompt should include raw input (identifier) before sanitization"
        );

        let out = filter.sanitize_prompt_text(&mut session, &prompt);
        assert!(
            !out.contains(secret),
            "sanitized prompt should not leak identifiers even when input contains fence markers: {out}"
        );
    }

    #[test]
    fn code_review_prompt_escapes_fence_markers_to_keep_sanitization_intact() {
        let cfg = AiPrivacyConfig {
            local_only: false,
            anonymize_identifiers: Some(true),
            ..AiPrivacyConfig::default()
        };
        let filter = PrivacyFilter::new(&cfg).expect("filter");
        let mut session = filter.new_session();

        let secret = "SecretService";
        let diff = format!(
            "diff --git a/src/Main.java b/src/Main.java\n\
--- a/src/Main.java\n\
+++ b/src/Main.java\n\
@@ -1,1 +1,1 @@\n\
-class Main {{}}\n\
+class Main {{ String s = \"```{secret}```\"; }}\n"
        );

        let prompt = crate::actions::code_review_prompt(&diff);
        assert!(prompt.contains(secret), "prompt should include raw input");

        let out = filter.sanitize_prompt_text(&mut session, &prompt);
        assert!(
            !out.contains(secret),
            "sanitized prompt should not leak identifiers even when diff contains fence markers: {out}"
        );
    }

    #[test]
    fn excluded_paths_relative_patterns_match_absolute_paths() {
        let cfg = AiPrivacyConfig {
            excluded_paths: vec!["secret/**".into()],
            ..AiPrivacyConfig::default()
        };

        let matcher = ExcludedPathMatcher::from_config(&cfg).expect("matcher");
        let filter = PrivacyFilter::new(&cfg).expect("filter");

        let abs = std::env::current_dir()
            .expect("cwd")
            .join("secret")
            .join("file.txt");

        assert!(
            matcher.is_match(&abs),
            "{abs:?} should match excluded_paths"
        );
        assert!(
            filter.is_excluded(&abs),
            "{abs:?} should be excluded via PrivacyFilter"
        );
    }

    #[test]
    fn excluded_paths_invalid_patterns_return_invalid_config() {
        let cfg = AiPrivacyConfig {
            excluded_paths: vec!["[unterminated".into()],
            ..AiPrivacyConfig::default()
        };

        let err = ExcludedPathMatcher::from_config(&cfg).expect_err("should fail");
        assert!(matches!(err, AiError::InvalidConfig(_)), "{err:?}");
    }

    #[test]
    fn excluded_paths_normalize_parent_dirs_in_relative_paths() {
        let cfg = AiPrivacyConfig {
            excluded_paths: vec!["secret/**".into()],
            ..AiPrivacyConfig::default()
        };

        let matcher = ExcludedPathMatcher::from_config(&cfg).expect("matcher");

        // `public/../secret/file.txt` should be treated the same as `secret/file.txt`.
        let rel = PathBuf::from("public")
            .join("..")
            .join("secret")
            .join("file.txt");
        assert!(
            matcher.is_match(&rel),
            "{rel:?} should match excluded_paths"
        );

        // Ensure we don't drop leading `..` segments for relative paths.
        let leading_parent = PathBuf::from("..").join("secret").join("file.txt");
        assert!(
            !matcher.is_match(&leading_parent),
            "{leading_parent:?} should not match secret/**"
        );
    }
}
