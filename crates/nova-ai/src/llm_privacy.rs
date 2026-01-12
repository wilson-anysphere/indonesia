use crate::anonymizer::{CodeAnonymizer, CodeAnonymizerOptions};
use crate::{types::CodeSnippet, AiError};
use globset::{Glob, GlobSet, GlobSetBuilder};
use nova_config::AiPrivacyConfig;
use regex::Regex;
use std::path::{Component, Path, PathBuf};

/// Privacy filtering for LLM backends configured via `nova-config`.
///
/// This sits alongside (and intentionally separate from) `nova_ai::privacy`,
/// which focuses on prompt-building and token redaction/anonymization heuristics.
pub struct PrivacyFilter {
    excluded_paths: GlobSet,
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
        let mut excluded_builder = GlobSetBuilder::new();
        for pattern in &config.excluded_paths {
            let glob = Glob::new(pattern).map_err(|err| {
                AiError::InvalidConfig(format!("invalid excluded_paths glob {pattern:?}: {err}"))
            })?;
            excluded_builder.add(glob);
        }

        let excluded_paths = excluded_builder.build().map_err(|err| {
            AiError::InvalidConfig(format!("failed to build excluded_paths globset: {err}"))
        })?;

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
        if self.excluded_paths.is_match(path) {
            return true;
        }

        // In real LSP usage we often receive absolute paths, while configuration globs are usually
        // written relative to the workspace root (e.g. `src/secrets/**`). `globset` matches from
        // the beginning of the path, so `src/secrets/**` won't match
        // `/home/user/project/src/secrets/Secret.java`.
        //
        // Best-effort fix: if the provided path is absolute, also attempt to match the configured
        // globs against each suffix of the path (dropping leading components). This preserves the
        // behavior of explicitly absolute patterns (e.g. `/home/user/**`) while avoiding false
        // negatives for workspace-relative globs.
        if path.is_absolute() {
            let components: Vec<Component<'_>> = path.components().collect();
            for start in 0..components.len() {
                if matches!(components[start], Component::Prefix(_) | Component::RootDir) {
                    continue;
                }

                let mut suffix = PathBuf::new();
                for component in &components[start..] {
                    suffix.push(component.as_os_str());
                }

                if self.excluded_paths.is_match(&suffix) {
                    return true;
                }
            }
        }

        false
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
        assert!(explain_prompt.contains(secret), "prompt should include raw input");
        let explain_out = filter.sanitize_prompt_text(&mut session, &explain_prompt);
        assert!(
            !explain_out.contains(secret),
            "sanitized prompt should not leak identifiers: {explain_out}"
        );

        let method_sig = format!("public {secret} make({secret} svc)");
        let method_prompt = crate::actions::generate_method_body_prompt(&method_sig, context);
        assert!(method_prompt.contains(secret), "prompt should include raw input");
        let method_out = filter.sanitize_prompt_text(&mut session, &method_prompt);
        assert!(
            !method_out.contains(secret),
            "sanitized prompt should not leak identifiers: {method_out}"
        );

        let target = format!("Generate tests for {secret}#run");
        let tests_prompt = crate::actions::generate_tests_prompt(&target, context);
        assert!(tests_prompt.contains(secret), "prompt should include raw input");
        let tests_out = filter.sanitize_prompt_text(&mut session, &tests_prompt);
        assert!(
            !tests_out.contains(secret),
            "sanitized prompt should not leak identifiers: {tests_out}"
        );
    }
}
