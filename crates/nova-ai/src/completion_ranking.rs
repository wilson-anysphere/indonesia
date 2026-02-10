use std::cmp::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::{BoxFuture, FutureExt};

use nova_config::AiConfig;
use nova_core::{CompletionContext, CompletionItem, CompletionItemKind};
use nova_fuzzy::{FuzzyMatcher, MatchScore};
use nova_metrics::MetricsRegistry;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::completion_ranking_prompt::CompletionRankingPromptBuilder;
use crate::privacy::redact_file_paths;
use crate::util;
use crate::{ChatMessage, ChatRequest, LlmClient};

pub(crate) const AI_COMPLETION_RANKING_METRIC: &str = "ai/completion_ranking";
// Used by LLM-backed rankers when they fall back due to provider/parse errors.
#[allow(dead_code)]
pub(crate) const AI_COMPLETION_RANKING_ERROR_METRIC: &str = "ai/completion_ranking/error";

/// Async-friendly interface for completion ranking.
///
/// Cancellation is achieved by dropping the returned future; `nova-lsp`/`nova-ide`
/// should always wrap ranking calls in a short timeout to avoid blocking
/// interactive requests.
pub trait CompletionRanker: Send + Sync {
    fn rank_completions<'a>(
        &'a self,
        ctx: &'a CompletionContext,
        items: Vec<CompletionItem>,
    ) -> BoxFuture<'a, Vec<CompletionItem>>;
}

/// A baseline, non-ML completion ranker.
///
/// This exists so the rest of the system can integrate AI "hooks" without
/// requiring a model. The heuristics are intentionally simple but deterministic.
#[derive(Debug, Default, Copy, Clone)]
pub struct BaselineCompletionRanker;

impl BaselineCompletionRanker {
    fn kind_bonus(kind: CompletionItemKind) -> i32 {
        match kind {
            CompletionItemKind::Method => 100,
            CompletionItemKind::Field => 80,
            CompletionItemKind::Variable => 70,
            CompletionItemKind::Class => 60,
            CompletionItemKind::Snippet => 50,
            CompletionItemKind::Keyword => 10,
            CompletionItemKind::Other => 0,
        }
    }
}

impl CompletionRanker for BaselineCompletionRanker {
    fn rank_completions<'a>(
        &'a self,
        ctx: &'a CompletionContext,
        mut items: Vec<CompletionItem>,
    ) -> BoxFuture<'a, Vec<CompletionItem>> {
        // Use the shared `nova-fuzzy` scorer so baseline ranking is consistent
        // with other non-AI fuzzy matching facilities in Nova.
        let mut matcher = FuzzyMatcher::new(&ctx.prefix);

        let mut scored: Vec<(CompletionItem, Option<MatchScore>, i32)> = items
            .drain(..)
            .map(|item| {
                let score = matcher.score(&item.label);
                let bonus = Self::kind_bonus(item.kind);
                (item, score, bonus)
            })
            .collect();

        scored.sort_by(|(a_item, a_score, a_bonus), (b_item, b_score, b_bonus)| {
            match (a_score, b_score) {
                (Some(a_score), Some(b_score)) => b_score
                    .rank_key()
                    .cmp(&a_score.rank_key())
                    .then_with(|| b_bonus.cmp(a_bonus))
                    .then_with(|| a_item.label.len().cmp(&b_item.label.len()))
                    .then_with(|| a_item.label.cmp(&b_item.label)),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => b_bonus
                    .cmp(a_bonus)
                    .then_with(|| a_item.label.len().cmp(&b_item.label.len()))
                    .then_with(|| a_item.label.cmp(&b_item.label)),
            }
        });

        let ranked: Vec<CompletionItem> = scored.into_iter().map(|(item, _, _)| item).collect();
        futures::future::ready(ranked).boxed()
    }
}

/// LLM-backed completion ranker.
///
/// This is designed to be cancellation friendly: dropping the returned future
/// cancels the in-flight LLM request via a request-scoped [`CancellationToken`].
#[derive(Clone)]
pub struct LlmCompletionRanker {
    llm: Arc<dyn LlmClient>,
    max_candidates: usize,
    max_prompt_chars: usize,
    max_label_chars: usize,
    max_detail_chars: usize,
    max_prefix_chars: usize,
    max_line_chars: usize,
    max_output_tokens: u32,
    timeout: Duration,
}

impl std::fmt::Debug for LlmCompletionRanker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmCompletionRanker")
            .field("max_candidates", &self.max_candidates)
            .field("max_prompt_chars", &self.max_prompt_chars)
            .field("max_label_chars", &self.max_label_chars)
            .field("max_detail_chars", &self.max_detail_chars)
            .field("max_prefix_chars", &self.max_prefix_chars)
            .field("max_line_chars", &self.max_line_chars)
            .field("max_output_tokens", &self.max_output_tokens)
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl LlmCompletionRanker {
    pub fn new(llm: Arc<dyn LlmClient>) -> Self {
        Self {
            llm,
            max_candidates: 20,
            max_prompt_chars: 8_192,
            max_label_chars: 120,
            max_detail_chars: 200,
            max_prefix_chars: 80,
            max_line_chars: 400,
            max_output_tokens: 96,
            timeout: Duration::from_millis(20),
        }
    }

    pub fn with_max_candidates(mut self, max: usize) -> Self {
        self.max_candidates = max;
        self
    }

    pub fn with_max_output_tokens(mut self, max: u32) -> Self {
        self.max_output_tokens = max;
        self
    }

    /// Override how long we're willing to wait for the model-backed ranking request.
    ///
    /// This is intended for latency-sensitive callers (e.g. LSP completion requests). On timeout,
    /// the ranker will gracefully fall back to deterministic local ranking.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn build_prompt(&self, ctx: &CompletionContext, candidates: &[CompletionItem]) -> Option<String> {
        let prompt = CompletionRankingPromptBuilder::new(self.max_prompt_chars)
            .with_max_label_chars(self.max_label_chars)
            .with_max_detail_chars(self.max_detail_chars)
            .with_max_prefix_chars(self.max_prefix_chars)
            .with_max_line_chars(self.max_line_chars)
            .build_prompt(ctx, candidates);

        if self.max_prompt_chars > 0 && prompt.len() > self.max_prompt_chars {
            return None;
        }

        Some(prompt)
    }
}

impl CompletionRanker for LlmCompletionRanker {
    fn rank_completions<'a>(
        &'a self,
        ctx: &'a CompletionContext,
        items: Vec<CompletionItem>,
    ) -> BoxFuture<'a, Vec<CompletionItem>> {
        Box::pin(async move {
            let metrics = MetricsRegistry::global();
            let metrics_start = Instant::now();

            let out = 'rank: {
                // Defensive limits: avoid prompting the model with huge lists.
                let rank_len = items.len().min(self.max_candidates);
                if rank_len <= 1 {
                    break 'rank items;
                }

                let mut to_rank = items;
                let rest = if to_rank.len() > rank_len {
                    to_rank.split_off(rank_len)
                } else {
                    Vec::new()
                };

                let prompt = match self.build_prompt(ctx, &to_rank) {
                    Some(prompt) => prompt,
                    None => {
                        // Prompt too large (or otherwise invalid): fall back to deterministic local
                        // ranking rather than returning the input order unchanged.
                        let mut all = to_rank;
                        all.extend(rest);
                        break 'rank BaselineCompletionRanker.rank_completions(ctx, all).await;
                    }
                };
                let prompt = redact_file_paths(&prompt);

                let cancel = CancellationToken::new();
                let _guard = cancel.clone().drop_guard();

                let request = ChatRequest {
                    messages: vec![
                        ChatMessage::system("Return JSON only.".to_string()),
                        ChatMessage::user(prompt),
                    ],
                    max_tokens: Some(self.max_output_tokens),
                    temperature: Some(0.0),
                };

                let chat_future = self.llm.chat(request, cancel.clone());
                let chat_future = std::panic::AssertUnwindSafe(chat_future).catch_unwind();

                let response = match util::timeout(self.timeout, chat_future).await {
                    Ok(Ok(Ok(text))) => text,
                    Ok(Ok(Err(_err))) => {
                        metrics.record_error(AI_COMPLETION_RANKING_METRIC);
                        metrics.record_error(AI_COMPLETION_RANKING_ERROR_METRIC);
                        let mut all = to_rank;
                        all.extend(rest);
                        break 'rank BaselineCompletionRanker.rank_completions(ctx, all).await;
                    }
                    Ok(Err(_panic)) => {
                        metrics.record_panic(AI_COMPLETION_RANKING_METRIC);
                        let mut all = to_rank;
                        all.extend(rest);
                        break 'rank BaselineCompletionRanker.rank_completions(ctx, all).await;
                    }
                    Err(_timeout) => {
                        metrics.record_timeout(AI_COMPLETION_RANKING_METRIC);
                        // Cancel eagerly so the in-flight request can abort while we compute the
                        // baseline result.
                        cancel.cancel();
                        let mut all = to_rank;
                        all.extend(rest);
                        break 'rank BaselineCompletionRanker.rank_completions(ctx, all).await;
                    }
                };

                let Some(order) = parse_ranked_ids(&response, to_rank.len()) else {
                    // Parse failures are treated the same as provider errors: preserve the original
                    // ordering (via baseline heuristics).
                    metrics.record_error(AI_COMPLETION_RANKING_METRIC);
                    metrics.record_error(AI_COMPLETION_RANKING_ERROR_METRIC);
                    let mut all = to_rank;
                    all.extend(rest);
                    break 'rank BaselineCompletionRanker.rank_completions(ctx, all).await;
                };

                let mut ranked = apply_rank_order(to_rank, &order);
                ranked.extend(rest);
                break 'rank ranked;
            };

            metrics.record_request(AI_COMPLETION_RANKING_METRIC, metrics_start.elapsed());
            out
        })
    }
}

// Prompt building/escaping/truncation helpers live in `completion_ranking_prompt.rs` so we have a
// single source of truth for privacy hardening.

fn parse_ranked_ids(text: &str, candidate_count: usize) -> Option<Vec<usize>> {
    let value = extract_first_json_array(text)?;
    let Value::Array(items) = value else {
        return None;
    };

    let mut out = Vec::<usize>::new();
    let mut seen = vec![false; candidate_count];
    for item in items {
        let Some(id) = item.as_i64() else {
            continue;
        };
        if id < 0 {
            continue;
        }
        let Ok(id) = usize::try_from(id) else {
            continue;
        };
        if id >= candidate_count {
            continue;
        }
        if seen[id] {
            continue;
        }
        seen[id] = true;
        out.push(id);
    }

    Some(out)
}

fn extract_first_json_array(text: &str) -> Option<Value> {
    // Fast-path: raw JSON.
    if let Ok(value) = serde_json::from_str::<Value>(text.trim()) {
        if matches!(value, Value::Array(_)) {
            return Some(value);
        }
    }

    // Robust path: search for the first substring that parses as a JSON array.
    let bytes = text.as_bytes();
    for start in 0..bytes.len() {
        if bytes[start] != b'[' {
            continue;
        }

        let mut depth: i32 = 0;
        for end in start..bytes.len() {
            match bytes[end] {
                b'[' => depth += 1,
                b']' => {
                    depth -= 1;
                    if depth == 0 {
                        let candidate = &text[start..=end];
                        if let Ok(value) = serde_json::from_str::<Value>(candidate) {
                            if matches!(value, Value::Array(_)) {
                                return Some(value);
                            }
                        }
                        break;
                    }
                }
                _ => {}
            }
        }
    }

    None
}

fn apply_rank_order(items: Vec<CompletionItem>, order: &[usize]) -> Vec<CompletionItem> {
    let mut remaining: Vec<Option<CompletionItem>> = items.into_iter().map(Some).collect();
    let mut out = Vec::with_capacity(remaining.len());

    for &id in order {
        if let Some(slot) = remaining.get_mut(id) {
            if let Some(item) = slot.take() {
                out.push(item);
            }
        }
    }

    // Append any missing candidates in original order.
    for item in remaining.into_iter().flatten() {
        out.push(item);
    }

    out
}

/// Run completion ranking with a timeout.
///
/// If ranking exceeds `timeout` (or panics), this returns `items` unchanged.
pub async fn rank_completions_with_timeout<R: CompletionRanker>(
    ranker: &R,
    ctx: &CompletionContext,
    items: Vec<CompletionItem>,
    timeout: Duration,
) -> Vec<CompletionItem> {
    let metrics = MetricsRegistry::global();
    let metrics_start = Instant::now();
    let fallback = items.clone();

    let ranked_future = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ranker.rank_completions(ctx, items)
    }));

    let out = match ranked_future {
        Ok(ranked_future) => {
            let ranked_future = std::panic::AssertUnwindSafe(ranked_future).catch_unwind();
            match util::timeout(timeout, ranked_future).await {
                Ok(Ok(ranked)) => ranked,
                Ok(Err(_panic)) => {
                    metrics.record_panic(AI_COMPLETION_RANKING_METRIC);
                    fallback
                }
                Err(_timeout) => {
                    metrics.record_timeout(AI_COMPLETION_RANKING_METRIC);
                    fallback
                }
            }
        }
        Err(_panic) => {
            metrics.record_panic(AI_COMPLETION_RANKING_METRIC);
            fallback
        }
    };

    metrics.record_request(AI_COMPLETION_RANKING_METRIC, metrics_start.elapsed());
    out
}

/// Convenience helper for integrating ranking behind feature flags.
///
/// When disabled (or if ranking fails/times out), this returns `items` unchanged.
pub async fn maybe_rank_completions<R: CompletionRanker>(
    config: &AiConfig,
    ranker: &R,
    ctx: &CompletionContext,
    items: Vec<CompletionItem>,
) -> Vec<CompletionItem> {
    if !(config.enabled && config.features.completion_ranking) {
        return items;
    }

    rank_completions_with_timeout(ranker, ctx, items, config.timeouts.completion_ranking()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completion_ranking_prompt::COMPLETION_RANKING_PROMPT_VERSION;
    use crate::llm_privacy::PrivacyFilter;
    use crate::{AiError, AiStream};
    use nova_config::AiPrivacyConfig;

    #[derive(Clone)]
    struct MockLlm {
        response: String,
        captured: Arc<std::sync::Mutex<Option<ChatRequest>>>,
    }

    impl MockLlm {
        fn new(response: impl Into<String>) -> Self {
            Self {
                response: response.into(),
                captured: Arc::new(std::sync::Mutex::new(None)),
            }
        }

        fn take_request(&self) -> Option<ChatRequest> {
            self.captured.lock().ok()?.take()
        }
    }

    #[derive(Clone, Default)]
    struct CapturingLlmClient {
        captured_cancel: Arc<std::sync::Mutex<Option<CancellationToken>>>,
    }

    #[async_trait::async_trait]
    impl LlmClient for CapturingLlmClient {
        async fn chat(
            &self,
            _request: ChatRequest,
            cancel: CancellationToken,
        ) -> Result<String, AiError> {
            *self
                .captured_cancel
                .lock()
                .expect("captured cancellation token mutex poisoned") = Some(cancel.clone());

            // Block forever unless the ranker drops/cancels the request token.
            cancel.cancelled().await;

            // If the ranker cancels early (e.g. via an incorrectly-scoped drop guard),
            // return an ordering that would change the output so tests can detect the
            // difference (i.e. this should not return the fallback ordering).
            Ok("[1,0]".to_string())
        }

        async fn chat_stream(
            &self,
            _request: ChatRequest,
            _cancel: CancellationToken,
        ) -> Result<AiStream, AiError> {
            Err(AiError::UnexpectedResponse(
                "streaming not supported for capturing mock".into(),
            ))
        }

        async fn list_models(&self, _cancel: CancellationToken) -> Result<Vec<String>, AiError> {
            Ok(Vec::new())
        }
    }

    #[async_trait::async_trait]
    impl LlmClient for MockLlm {
        async fn chat(
            &self,
            request: ChatRequest,
            _cancel: CancellationToken,
        ) -> Result<String, AiError> {
            *self.captured.lock().expect("captured request") = Some(request);
            Ok(self.response.clone())
        }

        async fn chat_stream(
            &self,
            _request: ChatRequest,
            _cancel: CancellationToken,
        ) -> Result<AiStream, AiError> {
            Err(AiError::UnexpectedResponse(
                "streaming not supported for mock".into(),
            ))
        }

        async fn list_models(&self, _cancel: CancellationToken) -> Result<Vec<String>, AiError> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn llm_ranker_reorders_candidates_from_json() {
        let _guard = crate::test_support::metrics_lock()
            .lock()
            .expect("metrics lock poisoned");
        let llm = Arc::new(MockLlm::new("[1,0,2]"));
        let ranker = LlmCompletionRanker::new(llm);

        let ctx = CompletionContext::new("pri", "System.out.");
        let items = vec![
            CompletionItem::new("private", CompletionItemKind::Keyword),
            CompletionItem::new("print", CompletionItemKind::Method),
            CompletionItem::new("println", CompletionItemKind::Method),
        ];

        let ranked = ranker.rank_completions(&ctx, items.clone()).await;
        assert_eq!(ranked[0].label, "print");
        assert_eq!(ranked[1].label, "private");
        assert_eq!(ranked[2].label, "println");
    }

    #[tokio::test]
    async fn llm_ranker_timeout_cancels_in_flight_llm_request() {
        let _guard = crate::test_support::metrics_lock()
            .lock()
            .expect("metrics lock poisoned");
        let llm = Arc::new(CapturingLlmClient::default());
        let ranker = LlmCompletionRanker::new(llm.clone());

        let ctx = CompletionContext::new("pri", "System.out.");
        let items = vec![
            CompletionItem::new("private", CompletionItemKind::Keyword),
            CompletionItem::new("print", CompletionItemKind::Method),
        ];

        let ranked = rank_completions_with_timeout(
            &ranker,
            &ctx,
            items.clone(),
            Duration::from_millis(1),
        )
        .await;

        // Ensure we really hit the timeout fallback. If the ranker ever returns early
        // (e.g. because the cancellation token was cancelled immediately), we'd see a
        // reordered output since the mock returns [1,0] after cancellation.
        assert_eq!(ranked, items);

        let cancel = llm
            .captured_cancel
            .lock()
            .expect("captured cancellation token mutex poisoned")
            .clone()
            .expect("expected LLM client to receive a cancellation token");

        assert!(
            cancel.is_cancelled(),
            "expected ranking timeout to drop the ranker future and cancel the in-flight LLM request"
        );
    }

    #[tokio::test]
    async fn llm_ranker_invalid_json_falls_back_to_baseline_ranking() {
        let _guard = crate::test_support::metrics_lock()
            .lock()
            .expect("metrics lock poisoned");
        let metrics = MetricsRegistry::global();
        let before = metrics.snapshot();
        let before_main = before
            .methods
            .get(AI_COMPLETION_RANKING_METRIC)
            .map(|m| m.error_count)
            .unwrap_or(0);
        let before_error = before
            .methods
            .get(AI_COMPLETION_RANKING_ERROR_METRIC)
            .map(|m| m.error_count)
            .unwrap_or(0);

        let llm = Arc::new(MockLlm::new("not json"));
        let ranker = LlmCompletionRanker::new(llm);

        let ctx = CompletionContext::new("p", "");
        let items = vec![
            CompletionItem::new("println", CompletionItemKind::Method),
            CompletionItem::new("print", CompletionItemKind::Method),
        ];

        let expected = BaselineCompletionRanker.rank_completions(&ctx, items.clone()).await;
        let ranked = ranker.rank_completions(&ctx, items.clone()).await;
        assert_eq!(ranked, expected);

        let after = metrics.snapshot();
        let after_main = after
            .methods
            .get(AI_COMPLETION_RANKING_METRIC)
            .map(|m| m.error_count)
            .unwrap_or(0);
        let after_error = after
            .methods
            .get(AI_COMPLETION_RANKING_ERROR_METRIC)
            .map(|m| m.error_count)
            .unwrap_or(0);

        assert!(
            after_main >= before_main.saturating_add(1),
            "expected {AI_COMPLETION_RANKING_METRIC} error_count to increment"
        );
        assert!(
            after_error >= before_error.saturating_add(1),
            "expected {AI_COMPLETION_RANKING_ERROR_METRIC} error_count to increment"
        );
    }

    #[tokio::test]
    async fn llm_ranker_missing_and_duplicate_ids_are_merged_gracefully() {
        let _guard = crate::test_support::metrics_lock()
            .lock()
            .expect("metrics lock poisoned");
        let llm = Arc::new(MockLlm::new("```json\n[1,1,0,99]\n```"));
        let ranker = LlmCompletionRanker::new(llm);

        let ctx = CompletionContext::new("p", "");
        let items = vec![
            CompletionItem::new("a", CompletionItemKind::Other),
            CompletionItem::new("b", CompletionItemKind::Other),
            CompletionItem::new("c", CompletionItemKind::Other),
        ];

        let ranked = ranker.rank_completions(&ctx, items.clone()).await;
        assert_eq!(ranked[0].label, "b");
        assert_eq!(ranked[1].label, "a");
        assert_eq!(ranked[2].label, "c");
    }

    #[tokio::test]
    async fn llm_ranker_uses_deterministic_request_parameters() {
        let _guard = crate::test_support::metrics_lock()
            .lock()
            .expect("metrics lock poisoned");
        let mock = MockLlm::new("[0]");
        let llm = Arc::new(mock.clone());
        let ranker = LlmCompletionRanker::new(llm.clone()).with_max_output_tokens(42);

        let ctx = CompletionContext::new("p", "x");
        let items = vec![
            CompletionItem::new("a", CompletionItemKind::Other),
            CompletionItem::new("b", CompletionItemKind::Other),
        ];

        let _ = ranker.rank_completions(&ctx, items).await;
        let req = mock.take_request().expect("request captured");
        assert_eq!(req.max_tokens, Some(42));
        assert_eq!(req.temperature, Some(0.0));
        assert!(
            req.messages
                .iter()
                .any(|m| m.content.contains(COMPLETION_RANKING_PROMPT_VERSION)),
            "expected prompt version marker in request"
        );
    }

    #[tokio::test]
    async fn llm_ranker_redacts_absolute_paths_in_prompt() {
        let _guard = crate::test_support::metrics_lock()
            .lock()
            .expect("metrics lock poisoned");
        let mock = MockLlm::new("[0]");
        let llm = Arc::new(mock.clone());
        let ranker = LlmCompletionRanker::new(llm);

        let line_text = r#"String a = "/home/alice/project/secret.txt"; String b = "C:\\Users\\alice\\secret.txt";"#;
        let ctx = CompletionContext::new("p", line_text);
        let items = vec![
            CompletionItem::new("a", CompletionItemKind::Other),
            CompletionItem::new("b", CompletionItemKind::Other),
        ];

        let _ = ranker.rank_completions(&ctx, items).await;
        let req = mock.take_request().expect("request captured");
        let prompt = req
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(prompt.contains("[PATH]"), "{prompt}");
        assert!(!prompt.contains("/home/alice/project/secret.txt"), "{prompt}");
        assert!(!prompt.contains(r"C:\\Users\\alice\\secret.txt"), "{prompt}");
    }

    #[tokio::test]
    async fn llm_ranker_includes_candidate_detail_in_prompt() {
        let _guard = crate::test_support::metrics_lock()
            .lock()
            .expect("metrics lock poisoned");
        let mock = MockLlm::new("[0,1]");
        let llm = Arc::new(mock.clone());
        let ranker = LlmCompletionRanker::new(llm);

        let ctx = CompletionContext::new("pri", "System.out.");
        let items = vec![
            CompletionItem::new("print", CompletionItemKind::Method)
                .with_detail("print(String value)"),
            CompletionItem::new("print", CompletionItemKind::Method).with_detail("print(int v)"),
        ];

        let _ = ranker.rank_completions(&ctx, items).await;
        let req = mock.take_request().expect("request captured");
        let prompt = req
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            prompt.contains("print(String value)"),
            "expected first detail signature to appear in prompt: {prompt}"
        );
        assert!(
            prompt.contains("print(int v)"),
            "expected second detail signature to appear in prompt: {prompt}"
        );
    }

    #[derive(Clone, Default)]
    struct ErrorLlm;

    #[async_trait::async_trait]
    impl LlmClient for ErrorLlm {
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
        ) -> Result<AiStream, AiError> {
            Err(AiError::UnexpectedResponse(
                "streaming not supported for mock".into(),
            ))
        }

        async fn list_models(&self, _cancel: CancellationToken) -> Result<Vec<String>, AiError> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn llm_ranker_provider_error_increments_error_metrics() {
        let _guard = crate::test_support::metrics_lock()
            .lock()
            .expect("metrics lock poisoned");
        let metrics = MetricsRegistry::global();
        let before = metrics.snapshot();
        let before_main = before
            .methods
            .get(AI_COMPLETION_RANKING_METRIC)
            .map(|m| m.error_count)
            .unwrap_or(0);
        let before_error = before
            .methods
            .get(AI_COMPLETION_RANKING_ERROR_METRIC)
            .map(|m| m.error_count)
            .unwrap_or(0);

        let llm = Arc::new(ErrorLlm::default());
        let ranker = LlmCompletionRanker::new(llm);

        let ctx = CompletionContext::new("p", "");
        let items = vec![
            CompletionItem::new("println", CompletionItemKind::Method),
            CompletionItem::new("print", CompletionItemKind::Method),
        ];

        let expected = BaselineCompletionRanker.rank_completions(&ctx, items.clone()).await;
        let ranked = ranker.rank_completions(&ctx, items.clone()).await;
        assert_eq!(ranked, expected);

        let after = metrics.snapshot();
        let after_main = after
            .methods
            .get(AI_COMPLETION_RANKING_METRIC)
            .map(|m| m.error_count)
            .unwrap_or(0);
        let after_error = after
            .methods
            .get(AI_COMPLETION_RANKING_ERROR_METRIC)
            .map(|m| m.error_count)
            .unwrap_or(0);

        assert!(
            after_main >= before_main.saturating_add(1),
            "expected {AI_COMPLETION_RANKING_METRIC} error_count to increment"
        );
        assert!(
            after_error >= before_error.saturating_add(1),
            "expected {AI_COMPLETION_RANKING_ERROR_METRIC} error_count to increment"
        );
    }

    #[test]
    fn completion_ranking_prompt_is_privacy_safe_against_markdown_fence_injection() {
        let secret = "SecretService";
        // Use 5 backticks; a naive `str::replace(\"```\", \"``\\\\`\")` is insufficient because it
        // can re-introduce a literal triple-backtick substring for longer runs.
        let injected = format!("`````{secret}");
        let ctx = CompletionContext::new(
            format!("my{injected}Prefix"),
            format!("var x = 0; {injected} x = null;"),
        );

        let candidates = vec![
            CompletionItem::new(format!("my{injected}Method"), CompletionItemKind::Method),
            CompletionItem::new(format!("my{injected}Field"), CompletionItemKind::Method),
        ];

        let llm = Arc::new(MockLlm::new("[0]"));
        let ranker = LlmCompletionRanker::new(llm);
        let prompt = ranker
            .build_prompt(&ctx, &candidates)
            .expect("prompt should be buildable");
        assert!(
            prompt.contains(secret),
            "raw prompt should contain user-derived identifier pre-sanitization"
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

        // Fence sanity: the prompt contains 3 fenced blocks (prefix/line/candidates).
        assert_eq!(
            sanitized.match_indices("```").count(),
            6,
            "expected exactly one opening+closing fence per section: {sanitized}"
        );

        // Candidate IDs must survive so the local caller can map the model's output.
        let marker = "Candidates:\n```java\n";
        let start = sanitized
            .find(marker)
            .unwrap_or_else(|| panic!("expected candidates section marker in prompt\n{sanitized}"));
        let after = &sanitized[start + marker.len()..];
        let end = after
            .find("```")
            .unwrap_or_else(|| panic!("expected closing candidates fence in prompt\n{sanitized}"));
        let block = &after[..end];
        assert!(
            block.lines().any(|line| line.starts_with("0:")),
            "expected candidate 0 to be present after sanitization\n{block}"
        );
        assert!(
            block.lines().any(|line| line.starts_with("1:")),
            "expected candidate 1 to be present after sanitization\n{block}"
        );
    }

    #[test]
    fn completion_ranking_prompt_builder_matches_llm_ranker_prompt() {
        let secret = "SecretService";
        let injected = format!("`````{secret}");
        let ctx = CompletionContext::new(
            format!("my{injected}Prefix"),
            format!("var x = 0; {injected} x = null; {}", "x".repeat(8_000)),
        );

        let candidates = vec![
            CompletionItem::new(format!("my{injected}Method"), CompletionItemKind::Method),
            CompletionItem::new(format!("my{injected}Field"), CompletionItemKind::Method),
        ];

        let llm = Arc::new(MockLlm::new("[0]"));
        let mut ranker = LlmCompletionRanker::new(llm);
        // Force truncation so we cover both the escape and prompt-size enforcement paths.
        ranker.max_prompt_chars = 800;

        let from_ranker = ranker
            .build_prompt(&ctx, &candidates)
            .expect("prompt should be buildable");
        assert!(
            from_ranker.len() <= ranker.max_prompt_chars,
            "ranker prompt should respect max_prompt_chars (got {} > {})",
            from_ranker.len(),
            ranker.max_prompt_chars
        );

        let from_builder = CompletionRankingPromptBuilder::new(ranker.max_prompt_chars)
            .with_max_label_chars(ranker.max_label_chars)
            .with_max_detail_chars(ranker.max_detail_chars)
            .with_max_prefix_chars(ranker.max_prefix_chars)
            .with_max_line_chars(ranker.max_line_chars)
            .build_prompt(&ctx, &candidates);
        assert_eq!(from_ranker, from_builder);
    }
}
