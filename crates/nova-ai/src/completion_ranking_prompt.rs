use std::borrow::Cow;

use nova_core::{CompletionContext, CompletionItem};

/// Deterministic prompt builder for model-backed completion ranking.
///
/// ## Privacy note: markdown fence injection hardening
///
/// `AiClient` sanitizes prompts in cloud mode via `PrivacyFilter::sanitize_prompt_text`, which
/// uses a deliberately tiny Markdown parser:
/// - It treats **any** occurrence of the substring `"```"` as a fence boundary.
///
/// If user-derived content (current line, prefix, candidate labels, etc) contains `"```"` inside a
/// fenced block, the sanitizer can accidentally "close" the fence early and treat the remainder as
/// plain text. In cloud mode, plain text is **not** identifier-anonymized, which can leak raw
/// project identifiers.
///
/// To prevent this, all user-derived strings that are embedded inside fenced blocks are passed
/// through `escape_markdown_fence_payload`, which guarantees the payload contains **no literal
/// `"```"` substring**, even for long runs like `"``````"`.
#[derive(Debug, Clone)]
pub struct CompletionRankingPromptBuilder {
    /// Upper bound for the prompt (in UTF-8 bytes). `0` disables truncation.
    ///
    /// Truncation is applied only to the *code body* portion so that the closing fence is always
    /// preserved (privacy filter relies on a well-formed fence to anonymize identifiers).
    max_prompt_chars: usize,
}

impl CompletionRankingPromptBuilder {
    pub fn new(max_prompt_chars: usize) -> Self {
        Self { max_prompt_chars }
    }

    /// Build a prompt containing:
    /// - The current completion context (prefix + current line)
    /// - The candidate list, with numeric IDs starting at 0
    ///
    /// All user-derived data is embedded in a fenced block so it can be anonymized/redacted by
    /// `PrivacyFilter` in cloud mode.
    pub fn build_prompt(&self, ctx: &CompletionContext, candidates: &[CompletionItem]) -> String {
        // Keep formatting stable for caching/tests; keep user-derived content inside the fence.
        const HEADER: &str = "You are Nova, a Java code completion ranking engine.\n\
Given the context and the completion candidates below, rank the candidates by relevance.\n\
Return JSON only in the form: {\"ranking\":[0,1,2]}.\n\n\
```java\n";
        // Always put the closing fence on its own line (preceded by a newline) so that a payload
        // ending in backticks cannot accidentally form a fence boundary by spanning the join.
        const FOOTER: &str = "\n```\n";

        let mut body = String::new();

        body.push_str("PREFIX:\n");
        body.push_str(escape_markdown_fence_payload(&ctx.prefix).as_ref());
        body.push('\n');

        body.push_str("LINE:\n");
        body.push_str(escape_markdown_fence_payload(&ctx.line_text).as_ref());
        body.push('\n');

        body.push_str("CANDIDATES:\n");
        for (id, item) in candidates.iter().enumerate() {
            body.push_str(&id.to_string());
            body.push_str(": ");
            body.push_str(escape_markdown_fence_payload(&item.label).as_ref());
            body.push('\n');
        }

        if self.max_prompt_chars > 0 {
            let fixed_len = HEADER.len() + FOOTER.len();
            if self.max_prompt_chars > fixed_len {
                let avail = self.max_prompt_chars - fixed_len;
                if body.len() > avail {
                    truncate_utf8_boundary(&mut body, avail);
                }
            }
        }

        let mut out = String::with_capacity(HEADER.len() + body.len() + FOOTER.len());
        out.push_str(HEADER);
        out.push_str(&body);
        out.push_str(FOOTER);
        out
    }
}

/// Escape arbitrary user-provided content so it cannot terminate a Markdown fenced code block.
///
/// This guarantees the returned string contains **no literal `"```"` substring** by inserting a
/// backslash before any backtick that would otherwise form 3 consecutive backticks.
fn escape_markdown_fence_payload(text: &str) -> Cow<'_, str> {
    // Fast path: if there's no triple-backtick substring, we don't need to allocate.
    if !text.contains("```") {
        return Cow::Borrowed(text);
    }

    let mut out = String::with_capacity(text.len() + text.len() / 2);
    let mut backticks = 0usize;

    for ch in text.chars() {
        if ch == '`' {
            if backticks == 2 {
                // Break the run before we would emit a third consecutive '`'.
                out.push('\\');
                backticks = 0;
            }
            out.push('`');
            backticks += 1;
        } else {
            out.push(ch);
            backticks = 0;
        }
    }

    debug_assert!(
        !out.contains("```"),
        "escape_markdown_fence_payload must remove all triple backticks"
    );

    Cow::Owned(out)
}

fn truncate_utf8_boundary(text: &mut String, max_bytes: usize) {
    if max_bytes >= text.len() {
        return;
    }

    let mut idx = max_bytes;
    while idx > 0 && !text.is_char_boundary(idx) {
        idx -= 1;
    }
    text.truncate(idx);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm_privacy::PrivacyFilter;
    use nova_config::AiPrivacyConfig;
    use nova_core::CompletionItemKind;

    #[test]
    fn escapes_triple_backticks_even_in_long_runs() {
        for input in ["```", "````", "``````", "a```b", "a````b", "a``````b"] {
            let escaped = escape_markdown_fence_payload(input);
            assert!(
                !escaped.contains("```"),
                "escaped payload should not contain triple backticks: input={input:?} escaped={escaped:?}"
            );
        }
    }

    #[test]
    fn completion_ranking_prompt_is_safe_against_fence_injection_for_privacy_filter() {
        let secret = "SecretService";
        let ctx = CompletionContext::new(
            "pri",
            // Simulate fence injection inside the current line.
            format!("var x = 0; ```{secret} x = null;"),
        );

        // Candidate list also includes the injection substring (rare, but must be safe).
        let candidates = vec![
            CompletionItem::new("println", CompletionItemKind::Method),
            CompletionItem::new(format!("````{secret}"), CompletionItemKind::Class),
        ];

        let prompt = CompletionRankingPromptBuilder::new(0).build_prompt(&ctx, &candidates);
        assert!(
            prompt.contains(secret),
            "prompt should contain raw user-derived identifier before sanitization"
        );

        let cfg = AiPrivacyConfig {
            local_only: false,
            anonymize_identifiers: Some(true),
            ..AiPrivacyConfig::default()
        };
        let filter = PrivacyFilter::new(&cfg).expect("privacy filter");
        let mut session = filter.new_session();

        let sanitized = filter.sanitize_prompt_text(&mut session, &prompt);

        assert!(
            !sanitized.contains(secret),
            "sanitized prompt must not leak identifiers in cloud mode: {sanitized}"
        );

        // Fence sanity: our prompt has exactly one fenced block; escaping ensures user content does
        // not accidentally create additional fences.
        assert!(sanitized.contains("```java\n"), "{sanitized}");
        assert_eq!(
            sanitized.match_indices("```").count(),
            2,
            "expected one opening and one closing fence: {sanitized}"
        );

        // Candidate IDs are protocol-critical; they must remain stable after sanitization.
        assert!(sanitized.contains("\n0:"), "{sanitized}");
        assert!(sanitized.contains("\n1:"), "{sanitized}");
    }

    #[test]
    fn prompt_truncation_never_drops_closing_fence() {
        let secret = "SecretService";
        let ctx = CompletionContext::new(
            "prefix",
            format!("```{secret} {}", "x".repeat(8_000)),
        );
        let candidates = vec![CompletionItem::new("foo", CompletionItemKind::Other)];

        // Force truncation.
        let max = 400usize;
        let prompt = CompletionRankingPromptBuilder::new(max).build_prompt(&ctx, &candidates);
        assert!(
            prompt.len() <= max,
            "prompt should respect max_prompt_chars (got {} > {max})",
            prompt.len()
        );
        assert!(prompt.contains(secret), "secret should still be present pre-sanitize");
        assert!(
            prompt.contains("\n```\n"),
            "prompt should always contain a closing fence even when truncated: {prompt}"
        );

        let cfg = AiPrivacyConfig {
            local_only: false,
            anonymize_identifiers: Some(true),
            ..AiPrivacyConfig::default()
        };
        let filter = PrivacyFilter::new(&cfg).expect("privacy filter");
        let mut session = filter.new_session();
        let sanitized = filter.sanitize_prompt_text(&mut session, &prompt);
        assert!(
            !sanitized.contains(secret),
            "sanitized prompt must not leak identifiers even when truncated: {sanitized}"
        );
    }
}

