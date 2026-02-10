use crate::{
    actions,
    client::{AiClient, LlmClient},
    context::{BuiltContext, ContextBuilder, ContextRequest},
    diff,
    types::{ChatMessage, ChatRequest, CodeSnippet},
    AiError,
};
use nova_config::AiConfig;
use nova_metrics::MetricsRegistry;
use std::{
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

const AI_ACTION_EXPLAIN_ERROR_METRIC: &str = "ai/action/explain_error";
const AI_ACTION_GENERATE_METHOD_BODY_METRIC: &str = "ai/action/generate_method_body";
const AI_ACTION_GENERATE_TESTS_METRIC: &str = "ai/action/generate_tests";
const AI_ACTION_CODE_REVIEW_METRIC: &str = "ai/action/code_review";

// Keep this as a benign string literal so identifier anonymization (cloud mode) won't rewrite it
// when sanitizing prompt context.
const EXCLUDED_PATHS_OMITTED_PLACEHOLDER: &str = "\"[some context omitted due to excluded_paths]\"";

fn record_action_metrics<T>(metric: &str, duration: Duration, result: &Result<T, AiError>) {
    let registry = MetricsRegistry::global();
    registry.record_request(metric, duration);

    if let Err(err) = result {
        registry.record_error(metric);
        if matches!(err, AiError::Timeout) || matches!(err, AiError::Http(inner) if inner.is_timeout()) {
            registry.record_timeout(metric);
        }
    }
}

pub struct NovaAi {
    client: Arc<AiClient>,
    llm: Arc<dyn LlmClient>,
    context_builder: ContextBuilder,
    max_output_tokens: u32,
    code_review_max_diff_chars: usize,
}

impl NovaAi {
    pub fn new(config: &AiConfig) -> Result<Self, AiError> {
        let client = Arc::new(AiClient::from_config(config)?);
        let llm: Arc<dyn LlmClient> = client.clone();

        Ok(Self {
            client,
            llm,
            context_builder: ContextBuilder::new(),
            max_output_tokens: config.provider.max_tokens,
            code_review_max_diff_chars: config.features.code_review_max_diff_chars.max(1),
        })
    }

    pub fn with_max_output_tokens(mut self, max: u32) -> Self {
        self.max_output_tokens = max;
        self
    }

    pub fn is_excluded_path(&self, path: &Path) -> bool {
        self.client.is_excluded_path(path)
    }

    fn sanitize_context_request_for_excluded_paths(
        &self,
        mut ctx: ContextRequest,
    ) -> ContextRequest {
        let mut omitted = false;

        ctx.extra_files.retain(|snippet| {
            let Some(path) = snippet.path.as_deref() else {
                return true;
            };
            if self.client.is_excluded_path(path) {
                omitted = true;
                return false;
            }
            true
        });

        ctx.related_code.retain(|related| {
            if self.client.is_excluded_path(&related.path) {
                omitted = true;
                return false;
            }
            true
        });

        if omitted {
            ctx.extra_files
                .push(CodeSnippet::ad_hoc(EXCLUDED_PATHS_OMITTED_PLACEHOLDER));
        }

        ctx
    }

    fn maybe_omit_context(&self, ctx: &ContextRequest, built: BuiltContext) -> BuiltContext {
        let Some(path) = ctx.file_path.as_deref() else {
            return built;
        };

        // Best-effort: treat `file_path` as a filesystem path (callers should avoid URIs here so
        // excluded_paths glob matching works).
        if self.client.is_excluded_path(Path::new(path)) {
            return BuiltContext {
                text: "[code context omitted due to excluded_paths]".to_string(),
                token_count: 0,
                truncated: true,
                sections: Vec::new(),
            };
        }

        built
    }

    fn explain_error_request(&self, diagnostic_message: &str, ctx: ContextRequest) -> ChatRequest {
        let ctx_sanitized = self.sanitize_context_request_for_excluded_paths(ctx.clone());
        let built = self.context_builder.build(ctx_sanitized);
        let BuiltContext { text, .. } = self.maybe_omit_context(&ctx, built);

        let user_prompt = actions::explain_error_prompt(diagnostic_message, &text);
        ChatRequest {
            messages: vec![
                ChatMessage::system("You are an expert Java developer assistant."),
                ChatMessage::user(user_prompt),
            ],
            max_tokens: Some(self.max_output_tokens),
            temperature: None,
        }
    }

    pub async fn explain_error(
        &self,
        diagnostic_message: &str,
        ctx: ContextRequest,
        cancel: CancellationToken,
    ) -> Result<String, AiError> {
        let started_at = Instant::now();
        let result = self
            .llm
            .chat(self.explain_error_request(diagnostic_message, ctx), cancel)
            .await;
        record_action_metrics(
            AI_ACTION_EXPLAIN_ERROR_METRIC,
            started_at.elapsed(),
            &result,
        );
        result
    }

    fn generate_method_body_request(
        &self,
        method_signature: &str,
        ctx: ContextRequest,
    ) -> ChatRequest {
        let ctx_sanitized = self.sanitize_context_request_for_excluded_paths(ctx.clone());
        let built = self.context_builder.build(ctx_sanitized);
        let BuiltContext { text, .. } = self.maybe_omit_context(&ctx, built);

        let user_prompt = actions::generate_method_body_prompt(method_signature, &text);
        ChatRequest {
            messages: vec![
                ChatMessage::system("You write correct, idiomatic Java code."),
                ChatMessage::user(user_prompt),
            ],
            max_tokens: Some(self.max_output_tokens),
            temperature: None,
        }
    }

    pub async fn generate_method_body(
        &self,
        method_signature: &str,
        ctx: ContextRequest,
        cancel: CancellationToken,
    ) -> Result<String, AiError> {
        let started_at = Instant::now();
        let result = self
            .llm
            .chat(
                self.generate_method_body_request(method_signature, ctx),
                cancel,
            )
            .await;
        record_action_metrics(
            AI_ACTION_GENERATE_METHOD_BODY_METRIC,
            started_at.elapsed(),
            &result,
        );
        result
    }

    fn generate_tests_request(&self, target: &str, ctx: ContextRequest) -> ChatRequest {
        let ctx_sanitized = self.sanitize_context_request_for_excluded_paths(ctx.clone());
        let built = self.context_builder.build(ctx_sanitized);
        let BuiltContext { text, .. } = self.maybe_omit_context(&ctx, built);

        let user_prompt = actions::generate_tests_prompt(target, &text);
        ChatRequest {
            messages: vec![
                ChatMessage::system("You are a meticulous Java test engineer."),
                ChatMessage::user(user_prompt),
            ],
            max_tokens: Some(self.max_output_tokens),
            temperature: None,
        }
    }

    pub async fn generate_tests(
        &self,
        target: &str,
        ctx: ContextRequest,
        cancel: CancellationToken,
    ) -> Result<String, AiError> {
        let started_at = Instant::now();
        let result = self
            .llm
            .chat(self.generate_tests_request(target, ctx), cancel)
            .await;
        record_action_metrics(
            AI_ACTION_GENERATE_TESTS_METRIC,
            started_at.elapsed(),
            &result,
        );
        result
    }

    pub async fn code_review(
        &self,
        diff: &str,
        cancel: CancellationToken,
    ) -> Result<String, AiError> {
        self.code_review_with_llm(self.llm.as_ref(), diff, cancel)
            .await
    }

    /// Like [`NovaAi::code_review`], but allows the caller (tests) to provide an alternate LLM
    /// client implementation.
    #[doc(hidden)]
    pub async fn code_review_with_llm(
        &self,
        llm: &dyn LlmClient,
        diff: &str,
        cancel: CancellationToken,
    ) -> Result<String, AiError> {
        let started_at = Instant::now();
        let filtered =
            diff::filter_diff_for_excluded_paths(diff, |path| self.client.is_excluded_path(path));

        let sanitized = self
            .client
            .sanitize_snippet(&CodeSnippet::ad_hoc(filtered.text))
            .unwrap_or_else(|| diff::DIFF_OMITTED_PLACEHOLDER.to_string());
        let sanitized = diff::replace_omission_sentinels(&sanitized);
        let sanitized = truncate_middle_with_marker(sanitized, self.code_review_max_diff_chars);

        let result = llm
            .chat(
                ChatRequest {
                    messages: vec![
                        ChatMessage::system("You are a senior Java engineer doing code review."),
                        ChatMessage::user(actions::code_review_prompt(&sanitized)),
                    ],
                    max_tokens: Some(self.max_output_tokens),
                    temperature: None,
                },
                cancel,
            )
            .await;
        record_action_metrics(AI_ACTION_CODE_REVIEW_METRIC, started_at.elapsed(), &result);
        result
    }

    /// Access the underlying client (for model listing, custom prompts, etc).
    pub fn llm(&self) -> Arc<dyn LlmClient> {
        self.llm.clone()
    }
}

fn truncate_middle_with_marker(text: String, max_chars: usize) -> String {
    let max_chars = max_chars.max(1);
    let total_chars = text.chars().count();
    if total_chars <= max_chars {
        return text;
    }

    if text.contains("diff --git ") {
        if let Some(out) = truncate_git_diff_by_file_sections(&text, total_chars, max_chars) {
            return out;
        }
        if let Some(out) = truncate_git_diff_by_hunk_boundaries(&text, total_chars, max_chars) {
            return out;
        }
    }

    truncate_by_lines_with_marker(&text, total_chars, max_chars)
}

const TRUNCATION_MARKER_PREFIX: &str = "\n\"[diff truncated: omitted ";
const TRUNCATION_MARKER_SUFFIX: &str = " chars]\"\n";
const TRUNCATION_MARKER_FALLBACK_INNER: &str = "[diff truncated]";

fn truncation_marker(omitted: usize) -> String {
    // Keep the marker as a benign string literal so identifier anonymization (cloud mode)
    // won't rewrite it when sanitizing the full prompt.
    format!("{TRUNCATION_MARKER_PREFIX}{omitted}{TRUNCATION_MARKER_SUFFIX}")
}

fn truncation_marker_len(omitted: usize) -> usize {
    TRUNCATION_MARKER_PREFIX.chars().count()
        + digit_count(omitted)
        + TRUNCATION_MARKER_SUFFIX.chars().count()
}

fn marker_only_within_limit(marker: &str, max_chars: usize) -> String {
    if marker.chars().count() <= max_chars {
        return marker.to_string();
    }

    if max_chars == 0 {
        return String::new();
    }

    // With a 1-character budget, we cannot produce a well-formed string literal (it needs both an
    // opening and closing quote). Return a best-effort hint that the marker is meant to be a
    // string literal so cloud-mode anonymization won't treat the marker contents as identifiers.
    if max_chars == 1 {
        return "\"".to_string();
    }

    let inner_budget = max_chars.saturating_sub(2);
    let inner = truncate_prefix_chars(TRUNCATION_MARKER_FALLBACK_INNER, inner_budget);
    format!("\"{inner}\"")
}

fn digit_count(mut n: usize) -> usize {
    let mut digits = 1usize;
    while n >= 10 {
        n /= 10;
        digits += 1;
    }
    digits
}

#[derive(Debug, Clone)]
struct DiffFileSection {
    start_line: usize,
    end_line: usize,
    chars: usize,
}

fn truncate_git_diff_by_file_sections(
    text: &str,
    total_chars: usize,
    max_chars: usize,
) -> Option<String> {
    // Preserve exact newlines by splitting inclusively.
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut line_chars: Vec<usize> = Vec::with_capacity(lines.len());
    for line in &lines {
        line_chars.push(line.chars().count());
    }

    let mut starts = Vec::<usize>::new();
    for (idx, line) in lines.iter().enumerate() {
        if line.starts_with("diff --git ") {
            starts.push(idx);
        }
    }

    // Need at least 2 file sections to omit at least one whole section while still keeping at
    // least one complete file boundary.
    if starts.len() < 2 {
        return None;
    }

    let preamble_end = starts[0];
    let preamble_chars: usize = line_chars[..preamble_end].iter().sum();

    let mut sections = Vec::<DiffFileSection>::with_capacity(starts.len());
    for (idx, &start_line) in starts.iter().enumerate() {
        let end_line = starts.get(idx + 1).copied().unwrap_or(lines.len());
        let chars: usize = line_chars[start_line..end_line].iter().sum();
        sections.push(DiffFileSection {
            start_line,
            end_line,
            chars,
        });
    }

    let section_count = sections.len();
    let mut prefix_chars = vec![0usize; section_count + 1];
    for i in 0..section_count {
        prefix_chars[i + 1] = prefix_chars[i] + sections[i].chars;
    }
    let mut suffix_chars = vec![0usize; section_count + 1];
    for i in 0..section_count {
        suffix_chars[i + 1] = suffix_chars[i] + sections[section_count - 1 - i].chars;
    }

    // Select the best (K, M) (keep first K sections and last M sections) that:
    // - omits at least one section (K + M < N)
    // - keeps at least one complete file section (K + M > 0)
    // - fits within max_chars once the marker is inserted
    //
    // Prefer keeping both a head and tail section when possible; otherwise, keep as many complete
    // file sections as we can (from either end).
    let mut best: Option<(usize, usize, usize, usize, bool)> = None; // (K, M, kept_chars, marker_len, has_both)
    for k in 0..=section_count {
        for m in 0..=section_count.saturating_sub(k) {
            let kept_sections = k + m;
            if kept_sections == 0 || kept_sections >= section_count {
                continue;
            }

            let kept = preamble_chars + prefix_chars[k] + suffix_chars[m];
            if kept >= total_chars {
                continue;
            }
            let omitted = total_chars - kept;
            let marker_len = truncation_marker_len(omitted);
            let out_len = kept + marker_len;
            if out_len > max_chars {
                continue;
            }

            let has_both = k > 0 && m > 0;
            match best {
                None => best = Some((k, m, kept, marker_len, has_both)),
                Some((best_k, best_m, best_kept, best_marker_len, best_has_both)) => {
                    let best_sections = best_k + best_m;
                    if (has_both && !best_has_both)
                        || (has_both == best_has_both && kept_sections > best_sections)
                        || (has_both == best_has_both
                            && kept_sections == best_sections
                            && kept > best_kept)
                        || (has_both == best_has_both
                            && kept_sections == best_sections
                            && kept == best_kept
                            && marker_len < best_marker_len)
                    {
                        best = Some((k, m, kept, marker_len, has_both));
                    }
                }
            }
        }
    }

    let (k, m, kept_chars, _marker_len, _has_both) = best?;
    let head_end_line = if k == 0 {
        preamble_end
    } else {
        sections[k - 1].end_line
    };
    let tail_start_line = if m == 0 {
        lines.len()
    } else {
        sections[section_count - m].start_line
    };

    let omitted = total_chars - kept_chars;
    let marker = truncation_marker(omitted);
    if max_chars <= marker.chars().count() {
        return Some(marker_only_within_limit(&marker, max_chars));
    }

    let mut out = String::with_capacity(text.len().min(max_chars) + marker.len());
    for line in &lines[..head_end_line] {
        out.push_str(line);
    }
    out.push_str(&marker);
    for line in &lines[tail_start_line..] {
        out.push_str(line);
    }
    Some(out)
}

#[derive(Debug, Clone)]
struct DiffHunkChunk {
    start_line: usize,
    end_line: usize,
    chars: usize,
}

/// Best-effort hunk-aware truncation for *single-file* git diffs.
///
/// When a diff contains only one `diff --git` section but many hunks, file-section truncation can't
/// apply. This tries to omit whole hunks (as chunk units) so truncation boundaries land between
/// hunks, preserving diff structure.
fn truncate_git_diff_by_hunk_boundaries(
    text: &str,
    total_chars: usize,
    max_chars: usize,
) -> Option<String> {
    // Preserve exact newlines by splitting inclusively.
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut line_chars: Vec<usize> = Vec::with_capacity(lines.len());
    for line in &lines {
        line_chars.push(line.chars().count());
    }

    let mut line_prefix_sum = vec![0usize; lines.len() + 1];
    for (idx, len) in line_chars.iter().enumerate() {
        line_prefix_sum[idx + 1] = line_prefix_sum[idx] + len;
    }

    let mut diff_starts = Vec::<usize>::new();
    let mut hunk_starts = Vec::<usize>::new();
    for (idx, line) in lines.iter().enumerate() {
        if line.starts_with("diff --git ") {
            diff_starts.push(idx);
        }
        if line.starts_with("@@") {
            hunk_starts.push(idx);
        }
    }

    // Only handle the single-file case here; multi-file diffs are handled by file-section
    // truncation above (and fall back to line-based truncation if whole-file preservation doesn't
    // fit the limit).
    if diff_starts.len() != 1 {
        return None;
    }
    let file_start = diff_starts[0];

    // Need enough hunks to omit something while keeping both ends.
    if hunk_starts.len() < 2 {
        return None;
    }

    let preamble_chars = line_prefix_sum[file_start];
    if preamble_chars >= max_chars {
        return None;
    }

    // Chunk boundaries: file start, each hunk start, and end of diff.
    let mut boundaries = Vec::<usize>::with_capacity(hunk_starts.len() + 2);
    boundaries.push(file_start);
    for idx in hunk_starts {
        if idx > file_start {
            boundaries.push(idx);
        }
    }
    boundaries.push(lines.len());
    boundaries.sort_unstable();
    boundaries.dedup();

    let mut chunks = Vec::<DiffHunkChunk>::new();
    for window in boundaries.windows(2) {
        let start = window[0];
        let end = window[1];
        if start >= end {
            continue;
        }
        let chars = line_prefix_sum[end] - line_prefix_sum[start];
        chunks.push(DiffHunkChunk {
            start_line: start,
            end_line: end,
            chars,
        });
    }

    if chunks.len() < 3 {
        return None;
    }

    let chunk_chars = chunks.iter().map(|chunk| chunk.chars).collect::<Vec<_>>();

    // Iterate until the marker length stabilizes (it depends on the omitted count's digit count).
    let mut marker_len = 0usize;
    let mut marker = String::new();
    for _ in 0..8 {
        let available_total = max_chars.saturating_sub(marker_len);
        if available_total <= preamble_chars {
            return None;
        }
        let available_chunks = available_total - preamble_chars;
        let (_prefix_chunks, _suffix_chunks, kept_chunk_chars) =
            select_prefix_suffix_lines(&chunk_chars, available_chunks);
        let kept_total_chars = preamble_chars + kept_chunk_chars;
        let omitted = total_chars.saturating_sub(kept_total_chars);
        let next_marker = truncation_marker(omitted);
        let next_len = next_marker.chars().count();
        marker = next_marker;
        if next_len == marker_len {
            break;
        }
        marker_len = next_len;
    }

    let marker_len = marker.chars().count();
    if max_chars <= marker_len {
        return Some(marker_only_within_limit(&marker, max_chars));
    }

    let available_total = max_chars - marker_len;
    if available_total <= preamble_chars {
        return None;
    }
    let available_chunks = available_total - preamble_chars;
    let (prefix_chunks, suffix_chunks, _kept_chunk_chars) =
        select_prefix_suffix_lines(&chunk_chars, available_chunks);
    if prefix_chunks == 0 || suffix_chunks == 0 || prefix_chunks + suffix_chunks >= chunks.len() {
        return None;
    }

    let head_end_line = chunks[prefix_chunks - 1].end_line;
    let tail_start_line = chunks[chunks.len().saturating_sub(suffix_chunks)].start_line;

    let mut out = String::with_capacity(text.len().min(max_chars) + marker.len());
    for line in &lines[..head_end_line] {
        out.push_str(line);
    }
    out.push_str(&marker);
    for line in &lines[tail_start_line..] {
        out.push_str(line);
    }
    Some(out)
}

fn truncate_by_lines_with_marker(text: &str, total_chars: usize, max_chars: usize) -> String {
    // Preserve exact newlines by splitting inclusively.
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    // For single-line input, splitting "by lines" gives us no better truncation boundary; fall
    // back to character-based truncation to ensure we still return some head/tail context.
    if lines.len() <= 1 {
        return truncate_chars_with_marker(text, total_chars, max_chars);
    }

    let mut line_chars: Vec<usize> = Vec::with_capacity(lines.len());
    for line in &lines {
        line_chars.push(line.chars().count());
    }

    // Iterate until the marker length stabilizes (it depends on the omitted count's digit count).
    let mut marker_len = 0usize;
    let mut marker = String::new();
    for _ in 0..8 {
        let available = max_chars.saturating_sub(marker_len);
        let (_prefix_lines, _suffix_lines, kept_chars) =
            select_prefix_suffix_lines(&line_chars, available);
        let omitted = total_chars.saturating_sub(kept_chars);
        let next_marker = truncation_marker(omitted);
        let next_len = next_marker.chars().count();
        marker = next_marker;
        if next_len == marker_len {
            break;
        }
        marker_len = next_len;
    }

    let marker_len = marker.chars().count();
    if max_chars <= marker_len {
        return marker_only_within_limit(&marker, max_chars);
    }

    // Recompute selection with the stabilized marker length to ensure we stay within max_chars.
    let available = max_chars - marker_len;
    let (prefix_lines, suffix_lines, _kept_chars) =
        select_prefix_suffix_lines(&line_chars, available);

    let suffix_start = lines.len().saturating_sub(suffix_lines);
    let mut out = String::with_capacity(text.len().min(max_chars) + marker.len());
    for line in &lines[..prefix_lines] {
        out.push_str(line);
    }
    out.push_str(&marker);
    for line in &lines[suffix_start..] {
        out.push_str(line);
    }
    out
}

fn select_prefix_suffix_lines(
    line_chars: &[usize],
    available_chars: usize,
) -> (usize, usize, usize) {
    // Choose a prefix of N lines and a suffix of M lines (non-overlapping) such that:
    // - sum(prefix) + sum(suffix) <= available_chars
    // - keeps at least one line from both ends when possible
    // - maximizes kept character count
    let n = line_chars.len();
    if n == 0 || available_chars == 0 {
        return (0, 0, 0);
    }

    let mut prefix_sum = vec![0usize; n + 1];
    for i in 0..n {
        prefix_sum[i + 1] = prefix_sum[i] + line_chars[i];
    }
    let mut suffix_sum = vec![0usize; n + 1];
    for i in 0..n {
        suffix_sum[i + 1] = suffix_sum[i] + line_chars[n - 1 - i];
    }

    let mut best_prefix = 0usize;
    let mut best_suffix = 0usize;
    let mut best_kept = 0usize;
    let mut best_has_both = false;

    for prefix_lines in 0..=n {
        let prefix_chars = prefix_sum[prefix_lines];
        if prefix_chars > available_chars {
            break;
        }
        let remaining = available_chars - prefix_chars;

        let max_suffix_allowed = n - prefix_lines;
        let mut suffix_lines = upper_bound_usize(&suffix_sum, remaining);
        if suffix_lines > max_suffix_allowed {
            suffix_lines = max_suffix_allowed;
        }

        let kept = prefix_chars + suffix_sum[suffix_lines];
        let has_both = prefix_lines > 0 && suffix_lines > 0;
        if (has_both && !best_has_both) || (has_both == best_has_both && kept > best_kept) {
            best_prefix = prefix_lines;
            best_suffix = suffix_lines;
            best_kept = kept;
            best_has_both = has_both;
        }
    }

    (best_prefix, best_suffix, best_kept)
}

fn upper_bound_usize(values: &[usize], max_value: usize) -> usize {
    // Return the largest `idx` such that `values[idx] <= max_value` (values must be sorted
    // ascending). Equivalent to `upper_bound - 1`, but returns 0 when no values fit.
    let mut lo = 0usize;
    let mut hi = values.len();
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if values[mid] <= max_value {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo.saturating_sub(1)
}

fn truncate_chars_with_marker(text: &str, total_chars: usize, max_chars: usize) -> String {
    // Iterate until the marker length stabilizes (it depends on the omitted count's digit count).
    let mut marker_len = 0usize;
    let mut marker = String::new();
    for _ in 0..8 {
        let available = max_chars.saturating_sub(marker_len);
        let omitted = total_chars.saturating_sub(available);
        let next_marker = truncation_marker(omitted);
        let next_len = next_marker.chars().count();
        marker = next_marker;
        if next_len == marker_len {
            break;
        }
        marker_len = next_len;
    }

    let marker_len = marker.chars().count();
    if max_chars <= marker_len {
        return marker_only_within_limit(&marker, max_chars);
    }

    let available = max_chars - marker_len;
    let head_len = available / 2;
    let tail_len = available - head_len;

    let head = truncate_prefix_chars(text, head_len);
    let tail = truncate_suffix_chars(text, tail_len);

    let mut out = String::with_capacity(max_chars.min(total_chars) + marker.len());
    out.push_str(head);
    out.push_str(&marker);
    out.push_str(tail);
    out
}

fn truncate_prefix_chars(text: &str, max_chars: usize) -> &str {
    if max_chars == 0 {
        return "";
    }

    match text.char_indices().nth(max_chars) {
        Some((idx, _)) => &text[..idx],
        None => text,
    }
}

fn truncate_suffix_chars(text: &str, max_chars: usize) -> &str {
    if max_chars == 0 {
        return "";
    }

    match text.char_indices().rev().nth(max_chars.saturating_sub(1)) {
        Some((idx, _)) => &text[idx..],
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        context::RelatedCode,
        privacy::{PrivacyMode, RedactionConfig},
    };
    use async_trait::async_trait;
    use nova_config::AiPrivacyConfig;
    use nova_metrics::MetricsRegistry;
    use std::path::PathBuf;

    fn minimal_ctx() -> ContextRequest {
        ContextRequest {
            file_path: None,
            focal_code: "class Main {}".to_string(),
            enclosing_context: None,
            project_context: None,
            semantic_context: None,
            related_symbols: Vec::new(),
            related_code: Vec::new(),
            cursor: None,
            diagnostics: Vec::new(),
            extra_files: Vec::new(),
            doc_comments: None,
            include_doc_comments: false,
            token_budget: 10_000,
            privacy: PrivacyMode::default(),
        }
    }

    #[test]
    fn truncation_marker_remains_string_literal_when_budget_is_tiny() {
        let diff = format!(
            "diff --git a/src/Main.java b/src/Main.java\n{}\n",
            "A".repeat(256)
        );

        let out = truncate_middle_with_marker(diff.clone(), 2);
        assert_eq!(out, "\"\"");

        let out = truncate_middle_with_marker(diff, 3);
        assert!(out.starts_with('"') && out.ends_with('"'), "{out}");
        assert!(out.chars().count() <= 3, "{out}");
    }

    #[test]
    fn max_tokens_defaults_to_provider_config() {
        let mut config = AiConfig::default();
        config.provider.max_tokens = 123;

        let ai = NovaAi::new(&config).expect("NovaAi should build with dummy config");
        let request = ai.explain_error_request("boom", minimal_ctx());

        assert_eq!(request.max_tokens, Some(123));
    }

    #[test]
    fn with_max_output_tokens_overrides_provider_config() {
        let mut config = AiConfig::default();
        config.provider.max_tokens = 123;

        let ai = NovaAi::new(&config)
            .expect("NovaAi should build with dummy config")
            .with_max_output_tokens(7);
        let request = ai.explain_error_request("boom", minimal_ctx());

        assert_eq!(request.max_tokens, Some(7));
    }

    #[test]
    fn excluded_paths_are_removed_from_related_code_and_extra_files_in_prompts() {
        let mut config = AiConfig::default();
        config.privacy = AiPrivacyConfig {
            excluded_paths: vec!["src/secrets/**".to_string()],
            ..AiPrivacyConfig::default()
        };

        let ai = NovaAi::new(&config).expect("NovaAi should build with dummy config");

        let secret_marker = "DO_NOT_LEAK_THIS_SECRET";
        let secret_code = format!("class Secret {{ String v = {secret_marker}; }}");
        let allowed_code = "class Helper {}".to_string();

        let ctx = ContextRequest {
            file_path: Some("src/Main.java".to_string()),
            focal_code: "class Main {}".to_string(),
            enclosing_context: None,
            project_context: None,
            semantic_context: None,
            related_symbols: Vec::new(),
            related_code: vec![
                RelatedCode {
                    path: PathBuf::from("src/secrets/Secret.java"),
                    range: 0..0,
                    kind: "class".to_string(),
                    snippet: secret_code.clone(),
                },
                RelatedCode {
                    path: PathBuf::from("src/Helper.java"),
                    range: 0..0,
                    kind: "class".to_string(),
                    snippet: allowed_code.clone(),
                },
            ],
            cursor: None,
            diagnostics: Vec::new(),
            extra_files: vec![
                CodeSnippet::new("src/secrets/Secret.java", secret_code.clone()),
                CodeSnippet::new("src/Helper.java", allowed_code.clone()),
            ],
            doc_comments: None,
            include_doc_comments: false,
            token_budget: 10_000,
            // Disable prompt-time anonymization/redaction so the test fails if the secret code is
            // included (we want omission, not masking).
            privacy: PrivacyMode {
                anonymize_identifiers: false,
                include_file_paths: true,
                redaction: RedactionConfig {
                    redact_string_literals: false,
                    redact_numeric_literals: false,
                    redact_comments: false,
                },
            },
        };

        let request = ai.explain_error_request("boom", ctx);
        let prompt = request
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            !prompt.contains(secret_marker),
            "excluded_paths code leaked into prompt: {prompt}"
        );
        assert!(
            prompt.contains("[some context omitted due to excluded_paths]"),
            "expected omission placeholder in prompt; got: {prompt}"
        );
        assert!(
            prompt.contains(&allowed_code),
            "expected allowed code to remain in prompt; got: {prompt}"
        );
    }

    #[test]
    fn excluded_paths_omission_marker_remains_readable_when_identifiers_are_anonymized() {
        let mut config = AiConfig::default();
        config.privacy = AiPrivacyConfig {
            excluded_paths: vec!["src/secrets/**".to_string()],
            ..AiPrivacyConfig::default()
        };

        let ai = NovaAi::new(&config).expect("NovaAi should build with dummy config");

        let secret_marker = "VERY_UNIQUE_MARKER_123";
        let secret_code = format!("class Secret {{ String v = \"{secret_marker}\"; }}");
        let allowed_marker = "ALLOWED_CONTEXT_MARKER";
        let allowed_code = format!("class Helper {{ String ok = \"{allowed_marker}\"; }}");

        let ctx = ContextRequest {
            file_path: Some("src/Main.java".to_string()),
            focal_code: "class Main {}".to_string(),
            enclosing_context: None,
            project_context: None,
            semantic_context: None,
            related_symbols: Vec::new(),
            related_code: vec![
                RelatedCode {
                    path: PathBuf::from("src/secrets/Secret.java"),
                    range: 0..0,
                    kind: "class".to_string(),
                    snippet: secret_code.clone(),
                },
                RelatedCode {
                    path: PathBuf::from("src/Helper.java"),
                    range: 0..0,
                    kind: "class".to_string(),
                    snippet: allowed_code.clone(),
                },
            ],
            cursor: None,
            diagnostics: Vec::new(),
            extra_files: vec![
                CodeSnippet::new("src/secrets/Secret.java", secret_code.clone()),
                CodeSnippet::new("src/Helper.java", allowed_code.clone()),
            ],
            doc_comments: None,
            include_doc_comments: false,
            token_budget: 10_000,
            // Cloud-default privacy mode: identifier anonymization + redaction enabled.
            privacy: PrivacyMode {
                anonymize_identifiers: true,
                include_file_paths: false,
                redaction: RedactionConfig::default(),
            },
        };

        let request = ai.explain_error_request("boom", ctx);
        let prompt = request
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            !prompt.contains(secret_marker),
            "excluded_paths code leaked into prompt: {prompt}"
        );
        assert!(
            prompt.contains("[some context omitted due to excluded_paths]"),
            "expected omission placeholder in prompt; got: {prompt}"
        );
        assert!(
            prompt.contains("excluded_paths"),
            "expected omission placeholder to remain human-readable; got: {prompt}"
        );
        assert!(
            prompt.contains(allowed_marker),
            "expected allowed context to remain in prompt; got: {prompt}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn explain_error_records_action_metrics_on_error() {
        #[derive(Debug, Clone)]
        struct MockLlm;

        #[async_trait]
        impl LlmClient for MockLlm {
            async fn chat(
                &self,
                _request: ChatRequest,
                _cancel: CancellationToken,
            ) -> Result<String, AiError> {
                Err(AiError::UnexpectedResponse("boom".to_string()))
            }

            async fn chat_stream(
                &self,
                _request: ChatRequest,
                _cancel: CancellationToken,
            ) -> Result<crate::types::AiStream, AiError> {
                Err(AiError::UnexpectedResponse("boom".to_string()))
            }

            async fn list_models(
                &self,
                _cancel: CancellationToken,
            ) -> Result<Vec<String>, AiError> {
                Ok(Vec::new())
            }
        }

        let _guard = crate::test_support::metrics_lock()
            .lock()
            .expect("metrics lock poisoned");

        let metrics = MetricsRegistry::global();
        metrics.reset();

        let config = AiConfig::default();
        let mut ai = NovaAi::new(&config).expect("NovaAi should build with dummy config");
        ai.llm = Arc::new(MockLlm);

        let ctx = ContextRequest {
            file_path: None,
            focal_code: "class Main {}".to_string(),
            enclosing_context: None,
            project_context: None,
            semantic_context: None,
            related_symbols: Vec::new(),
            related_code: Vec::new(),
            cursor: None,
            diagnostics: Vec::new(),
            extra_files: Vec::new(),
            doc_comments: None,
            include_doc_comments: false,
            token_budget: 10_000,
            privacy: PrivacyMode::default(),
        };

        let err = ai
            .explain_error("diagnostic", ctx, CancellationToken::new())
            .await
            .expect_err("expected mock error");
        assert!(matches!(err, AiError::UnexpectedResponse(_)));

        let snap = metrics.snapshot();
        let method = snap
            .methods
            .get(AI_ACTION_EXPLAIN_ERROR_METRIC)
            .expect("action metric present");
        assert_eq!(method.request_count, 1);
        assert_eq!(method.error_count, 1);

        metrics.reset();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn explain_error_records_action_metrics_on_timeout() {
        #[derive(Debug, Clone)]
        struct TimeoutLlm;

        #[async_trait]
        impl LlmClient for TimeoutLlm {
            async fn chat(
                &self,
                _request: ChatRequest,
                _cancel: CancellationToken,
            ) -> Result<String, AiError> {
                Err(AiError::Timeout)
            }

            async fn chat_stream(
                &self,
                _request: ChatRequest,
                _cancel: CancellationToken,
            ) -> Result<crate::types::AiStream, AiError> {
                Err(AiError::Timeout)
            }

            async fn list_models(
                &self,
                _cancel: CancellationToken,
            ) -> Result<Vec<String>, AiError> {
                Ok(Vec::new())
            }
        }

        let _guard = crate::test_support::metrics_lock()
            .lock()
            .expect("metrics lock poisoned");

        let metrics = MetricsRegistry::global();
        metrics.reset();

        let config = AiConfig::default();
        let mut ai = NovaAi::new(&config).expect("NovaAi should build with dummy config");
        ai.llm = Arc::new(TimeoutLlm);

        let err = ai
            .explain_error("diagnostic", minimal_ctx(), CancellationToken::new())
            .await
            .expect_err("expected timeout");
        assert!(matches!(err, AiError::Timeout));

        let snap = metrics.snapshot();
        let method = snap
            .methods
            .get(AI_ACTION_EXPLAIN_ERROR_METRIC)
            .expect("action metric present");
        assert_eq!(method.request_count, 1);
        assert_eq!(method.error_count, 1);
        assert_eq!(method.timeout_count, 1);

        metrics.reset();
    }
}
