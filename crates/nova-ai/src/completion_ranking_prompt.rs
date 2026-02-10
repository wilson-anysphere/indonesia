use nova_core::{CompletionContext, CompletionItem, CompletionItemKind};

use crate::util::markdown::escape_markdown_fence_payload;

pub(crate) const COMPLETION_RANKING_PROMPT_VERSION: &str = "nova_completion_ranking_v1";

/// Deterministic prompt builder for model-backed completion ranking.
///
/// ## Privacy note: markdown fence injection hardening
///
/// `AiClient` sanitizes prompts in cloud mode via `PrivacyFilter::sanitize_prompt_text`, which
/// uses a deliberately tiny Markdown fence parser:
/// - It recognizes backtick fences with 3+ backticks (```) only when they appear at the start of a
///   line (after optional indentation).
/// - Closing fences must also be line-start and contain only optional whitespace after the backtick
///   run.
///
/// If user-derived content (current line, prefix, candidate labels, etc) contains a line that looks
/// like a fence delimiter (e.g. starts with ```` ``` ````), the sanitizer can "close" the fence
/// early and treat the remainder as plain text. In cloud mode, plain text is **not**
/// identifier-anonymized, which can leak raw project identifiers.
///
/// To prevent this, all user-derived strings that are embedded inside fenced blocks are passed
/// through `escape_markdown_fence_payload`, which guarantees the payload contains **no literal
/// `"```"` substring**, even for long runs like `"``````"`.
#[derive(Debug, Clone)]
pub struct CompletionRankingPromptBuilder {
    /// Upper bound for the prompt (in UTF-8 bytes). `0` disables truncation.
    ///
    /// Truncation is applied only to the *code body* portions so that all closing fences are always
    /// preserved (privacy filter relies on well-formed fences to anonymize identifiers).
    max_prompt_chars: usize,
    max_label_chars: usize,
    max_detail_chars: usize,
    max_prefix_chars: usize,
    max_line_chars: usize,
}

impl CompletionRankingPromptBuilder {
    pub fn new(max_prompt_chars: usize) -> Self {
        Self {
            max_prompt_chars,
            // These defaults are intentionally kept in sync with `LlmCompletionRanker` so the
            // builder can be used as a standalone prompt generator while still producing the
            // same prompt shape/limits as the ranker.
            max_label_chars: 120,
            max_detail_chars: 200,
            max_prefix_chars: 80,
            max_line_chars: 400,
        }
    }

    pub fn with_max_label_chars(mut self, max: usize) -> Self {
        self.max_label_chars = max;
        self
    }

    pub fn with_max_detail_chars(mut self, max: usize) -> Self {
        self.max_detail_chars = max;
        self
    }

    pub fn with_max_prefix_chars(mut self, max: usize) -> Self {
        self.max_prefix_chars = max;
        self
    }

    pub fn with_max_line_chars(mut self, max: usize) -> Self {
        self.max_line_chars = max;
        self
    }

    /// Build a prompt containing:
    /// - The current completion context (prefix + current line)
    /// - The candidate list, with numeric IDs starting at 0
    ///
    /// All user-derived data is embedded in fenced blocks so it can be anonymized/redacted by
    /// `PrivacyFilter` in cloud mode.
    pub fn build_prompt(&self, ctx: &CompletionContext, candidates: &[CompletionItem]) -> String {
        let mut prefix = sanitize_code_block(&ctx.prefix, self.max_prefix_chars);
        let mut line_text = sanitize_code_block(&ctx.line_text, self.max_line_chars);
        let mut candidates_text =
            sanitize_candidates(candidates, self.max_label_chars, self.max_detail_chars);

        if self.max_prompt_chars > 0 {
            truncate_prompt_payloads(
                self.max_prompt_chars,
                &mut prefix,
                &mut line_text,
                &mut candidates_text,
            );
        }

        let mut out = String::with_capacity(1024);
        out.push_str(INTRO_ENGINE);
        out.push_str(INTRO_VERSION_PREFIX);
        out.push_str(COMPLETION_RANKING_PROMPT_VERSION);
        out.push_str(INTRO_VERSION_SUFFIX);
        out.push_str(INTRO_TASK);
        out.push_str(INTRO_RETURN_JSON_ARRAY);
        out.push_str(INTRO_EXAMPLE);

        out.push_str(PREFIX_INTRO);
        out.push_str(&prefix);
        out.push_str(PREFIX_OUTRO);

        out.push_str(LINE_INTRO);
        out.push_str(&line_text);
        out.push_str(LINE_OUTRO);

        out.push_str(CANDIDATES_INTRO);
        out.push_str(&candidates_text);
        out.push_str(CANDIDATES_OUTRO);

        out.push_str(OUTRO_RETURN_JSON_ONLY);

        debug_assert!(
            self.max_prompt_chars == 0 || out.len() <= self.max_prompt_chars,
            "completion ranking prompt builder must respect max_prompt_chars",
        );

        out
    }
}

const INTRO_ENGINE: &str = "You are a Java code completion ranking engine.\n";
const INTRO_VERSION_PREFIX: &str = "Prompt version: ";
const INTRO_VERSION_SUFFIX: &str = "\n\n";
const INTRO_TASK: &str = "Task: rank the candidate completion items from best to worst.\n";
const INTRO_RETURN_JSON_ARRAY: &str =
    "Return ONLY a JSON array of candidate IDs (integers) in best-to-worst order.\n";
const INTRO_EXAMPLE: &str = "Example: [1,0,2]\n\n";

const PREFIX_INTRO: &str = "User prefix:\n```java\n";
// Always put the closing fence on its own line (preceded by a newline) so that a payload ending in
// backticks cannot accidentally form a fence boundary by spanning the join.
const PREFIX_OUTRO: &str = "\n```\n\n";

const LINE_INTRO: &str = "Current line:\n```java\n";
const LINE_OUTRO: &str = "\n```\n\n";

const CANDIDATES_INTRO: &str = "Candidates:\n```java\n";
const CANDIDATES_OUTRO: &str = "\n```\n\n";

const OUTRO_RETURN_JSON_ONLY: &str = "Return JSON only. No markdown, no explanation.\n";

fn sanitize_candidates(
    candidates: &[CompletionItem],
    max_label_chars: usize,
    max_detail_chars: usize,
) -> String {
    let mut out = String::with_capacity(candidates.len() * 64);
    for (id, item) in candidates.iter().enumerate() {
        if id > 0 {
            out.push('\n');
        }
        let label = sanitize_label(&item.label, max_label_chars);
        let detail = item
            .detail
            .as_deref()
            .and_then(|detail| sanitize_detail(detail, max_detail_chars));
        // Kind is not sensitive but keep the whole candidate payload within the code fence so
        // identifier anonymization/redaction can safely apply to labels.
        out.push_str(&format!(
            "{id}: {} {label}",
            completion_kind_label(item.kind)
        ));
        if let Some(detail) = detail {
            out.push_str(" — ");
            out.push_str(&detail);
        }
    }
    out
}

fn sanitize_label(label: &str, max_chars: usize) -> String {
    // Labels are expected to be single-line but be defensive.
    let out = label.replace(['\n', '\r'], " ");
    let escaped = escape_markdown_fence_payload(&out);
    truncate_chars(escaped.as_ref(), max_chars)
}

fn sanitize_detail(detail: &str, max_chars: usize) -> Option<String> {
    let detail = detail.trim();
    if detail.is_empty() {
        return None;
    }

    // File paths are considered sensitive. Drop detail strings that look like filesystem paths
    // (LSP servers sometimes include file locations in `.detail`).
    if detail.contains('/') || detail.contains('\\') {
        return None;
    }

    // Details are usually single-line but be defensive.
    let out = detail.replace(['\n', '\r'], " ");
    let escaped = escape_markdown_fence_payload(&out);
    Some(truncate_chars(escaped.as_ref(), max_chars))
}

fn sanitize_code_block(text: &str, max_chars: usize) -> String {
    let escaped = escape_markdown_fence_payload(text);
    truncate_chars(escaped.as_ref(), max_chars)
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    text.chars().take(max_chars).collect()
}

fn completion_kind_label(kind: CompletionItemKind) -> &'static str {
    match kind {
        CompletionItemKind::Keyword => "Keyword",
        CompletionItemKind::Class => "Class",
        CompletionItemKind::Method => "Method",
        CompletionItemKind::Field => "Field",
        CompletionItemKind::Variable => "Variable",
        CompletionItemKind::Snippet => "Snippet",
        CompletionItemKind::Other => "Other",
    }
}

fn truncate_prompt_payloads(
    max_prompt_chars: usize,
    prefix: &mut String,
    line_text: &mut String,
    candidates_text: &mut String,
) {
    let fixed_len = prompt_fixed_len();
    if max_prompt_chars <= fixed_len {
        // No amount of payload truncation can make the prompt fit. Keep the payload as-is so
        // callers can decide whether to fall back.
        return;
    }

    let budget = max_prompt_chars - fixed_len;
    let payload_len = prefix.len() + line_text.len() + candidates_text.len();
    if payload_len <= budget {
        return;
    }

    let mut overflow = payload_len - budget;

    // Prefer keeping candidates intact; truncate context first. However, keep a small amount of the
    // current-line context when possible so truncation doesn't completely eliminate the substring
    // that would have triggered fence-injection escaping (keeps privacy regression tests
    // meaningful).
    const MIN_LINE_CONTEXT_BYTES: usize = 20;
    overflow = truncate_string_by_bytes(line_text, overflow, MIN_LINE_CONTEXT_BYTES);
    overflow = truncate_string_by_bytes(prefix, overflow, 0);
    overflow = truncate_string_by_bytes(candidates_text, overflow, 0);

    // If we still overflow (extremely small max prompt), we have to violate our minimums.
    if overflow > 0 {
        overflow = truncate_string_by_bytes(line_text, overflow, 0);
        overflow = truncate_string_by_bytes(prefix, overflow, 0);
        let _ = truncate_string_by_bytes(candidates_text, overflow, 0);
    }
}

fn truncate_string_by_bytes(text: &mut String, overflow: usize, min_bytes: usize) -> usize {
    if overflow == 0 || text.is_empty() {
        return overflow;
    }

    let original_len = text.len();
    let min_bytes = min_bytes.min(original_len);
    let mut min_boundary = min_bytes;
    while min_boundary > 0 && !text.is_char_boundary(min_boundary) {
        min_boundary -= 1;
    }

    if original_len <= min_boundary {
        return overflow;
    }

    let max_reducible = original_len - min_boundary;
    let reduce_by = overflow.min(max_reducible);

    let desired_len = original_len - reduce_by;
    let mut idx = desired_len;
    while idx > min_boundary && !text.is_char_boundary(idx) {
        idx -= 1;
    }
    text.truncate(idx);
    let reduced = original_len - text.len();
    overflow.saturating_sub(reduced)
}

fn prompt_fixed_len() -> usize {
    INTRO_ENGINE.len()
        + INTRO_VERSION_PREFIX.len()
        + COMPLETION_RANKING_PROMPT_VERSION.len()
        + INTRO_VERSION_SUFFIX.len()
        + INTRO_TASK.len()
        + INTRO_RETURN_JSON_ARRAY.len()
        + INTRO_EXAMPLE.len()
        + PREFIX_INTRO.len()
        + PREFIX_OUTRO.len()
        + LINE_INTRO.len()
        + LINE_OUTRO.len()
        + CANDIDATES_INTRO.len()
        + CANDIDATES_OUTRO.len()
        + OUTRO_RETURN_JSON_ONLY.len()
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
            CompletionItem::new("println", CompletionItemKind::Method)
                .with_detail(format!("void println(```{secret})")),
            CompletionItem::new(format!("````{secret}"), CompletionItemKind::Class)
                .with_detail(format!("`````{secret}Detail")),
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

        // Fence sanity: our prompt has 3 fenced blocks (prefix/current line/candidates); escaping
        // ensures user content does not accidentally create additional fences.
        assert!(sanitized.contains("```java\n"), "{sanitized}");
        assert_eq!(
            sanitized.match_indices("```").count(),
            6,
            "expected one opening+closing fence per section: {sanitized}"
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
            prompt.match_indices("\n```\n").count() >= 3,
            "prompt should always contain closing fences even when truncated: {prompt}"
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
