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

fn record_action_metrics<T>(metric: &str, duration: Duration, result: &Result<T, AiError>) {
    let registry = MetricsRegistry::global();
    registry.record_request(metric, duration);

    if let Err(err) = result {
        registry.record_error(metric);
        if matches!(err, AiError::Timeout) {
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
            ctx.extra_files.push(CodeSnippet::ad_hoc(
                "[some context omitted due to excluded_paths]",
            ));
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
        self.code_review_with_llm(self.llm.as_ref(), diff, cancel).await
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
        let filtered = diff::filter_diff_for_excluded_paths(diff, |path| {
            self.client.is_excluded_path(path)
        });

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
        record_action_metrics(
            AI_ACTION_CODE_REVIEW_METRIC,
            started_at.elapsed(),
            &result,
        );
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

    // Iterate until the marker length stabilizes (it depends on the omitted count's digit count).
    let mut marker_len = 0usize;
    let mut marker = String::new();
    for _ in 0..8 {
        let available = max_chars.saturating_sub(marker_len);
        let omitted = total_chars.saturating_sub(available);
        let next_marker = format!("\n[diff truncated: omitted {omitted} chars]\n");
        let next_len = next_marker.chars().count();
        marker = next_marker;
        if next_len == marker_len {
            break;
        }
        marker_len = next_len;
    }

    let marker_len = marker.chars().count();
    if max_chars <= marker_len {
        return truncate_prefix_chars(&marker, max_chars).to_string();
    }

    let available = max_chars - marker_len;
    let head_len = available / 2;
    let tail_len = available - head_len;

    let head = truncate_prefix_chars(&text, head_len);
    let tail = truncate_suffix_chars(&text, tail_len);

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
