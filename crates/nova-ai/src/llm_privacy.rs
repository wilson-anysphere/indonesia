use crate::anonymizer::{CodeAnonymizer, CodeAnonymizerOptions};
use crate::{types::CodeSnippet, AiError};
use globset::{Glob, GlobSet, GlobSetBuilder};
use nova_config::AiPrivacyConfig;
use regex::Regex;
use std::path::Path;

/// Privacy filtering for LLM backends configured via `nova-config`.
///
/// This sits alongside (and intentionally separate from) `nova_ai::privacy`,
/// which focuses on prompt-building and token redaction/anonymization heuristics.
pub struct PrivacyFilter {
    excluded_paths: GlobSet,
    redact_patterns: Vec<Regex>,
    anonymize_code: bool,
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
    fn new(anonymize_code: bool) -> Self {
        let options = CodeAnonymizerOptions {
            anonymize_identifiers: anonymize_code,
            // These are only enabled when anonymization is enabled. When
            // everything stays local we avoid extra transformations.
            redact_sensitive_strings: anonymize_code,
            redact_numeric_literals: anonymize_code,
            strip_or_redact_comments: anonymize_code,
        };
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

        Ok(Self {
            excluded_paths,
            redact_patterns,
            anonymize_code: config.effective_anonymize(),
        })
    }

    pub(crate) fn new_session(&self) -> SanitizationSession {
        SanitizationSession::new(self.anonymize_code)
    }

    pub fn is_excluded(&self, path: &Path) -> bool {
        self.excluded_paths.is_match(path)
    }

    /// Apply redaction patterns to arbitrary prompt text.
    pub fn sanitize_prompt_text(&self, session: &mut SanitizationSession, text: &str) -> String {
        sanitize_markdown_fenced_code_blocks(
            text,
            |block| {
                let sanitized = if self.anonymize_code {
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
        let sanitized = if self.anonymize_code {
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
            anonymize: Some(true),
            excluded_paths: Vec::new(),
            redact_patterns: Vec::new(),
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
}
