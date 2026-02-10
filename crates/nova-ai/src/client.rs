use crate::{
    audit,
    cache::{shared_cache, CacheKey, CacheKeyBuilder, CacheSettings, LlmResponseCache},
    cloud::{AnthropicProvider, AzureOpenAiProvider, GeminiProvider, HttpProvider},
    llm_privacy::PrivacyFilter,
    providers::{ollama::OllamaProvider, openai_compatible::OpenAiCompatibleProvider, LlmProvider},
    types::{AiStream, ChatMessage, ChatRequest, CodeSnippet},
    AiError,
};
use async_trait::async_trait;
use futures::StreamExt;
use nova_config::{AiConfig, AiProviderKind};
use nova_metrics::MetricsRegistry;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex as TokioMutex, Notify, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};
use url::Host;

#[cfg(feature = "local-llm")]
use crate::providers::in_process_llama::InProcessLlamaProvider;

#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn chat(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<String, AiError>;

    async fn chat_stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<AiStream, AiError>;

    async fn list_models(&self, cancel: CancellationToken) -> Result<Vec<String>, AiError>;

    async fn generate(&self, prompt: String, cancel: CancellationToken) -> Result<String, AiError> {
        self.chat(
            ChatRequest {
                messages: vec![ChatMessage::user(prompt)],
                max_tokens: None,
                temperature: None,
            },
            cancel,
        )
        .await
    }
}

#[derive(Debug, Clone)]
struct RetryConfig {
    max_retries: usize,
    initial_backoff: Duration,
    max_backoff: Duration,
}

const AI_CHAT_METRIC: &str = "ai/chat";
const AI_CHAT_CACHE_HIT_METRIC: &str = "ai/chat/cache_hit";
const AI_CHAT_CACHE_MISS_METRIC: &str = "ai/chat/cache_miss";
const AI_CHAT_RETRY_METRIC: &str = "ai/chat/retry";
const AI_CHAT_COALESCED_WAITER_METRIC: &str = "ai/chat/coalesced_waiter";

const AI_CHAT_ERROR_TIMEOUT_METRIC: &str = "ai/chat/error/timeout";
const AI_CHAT_ERROR_CANCELLED_METRIC: &str = "ai/chat/error/cancelled";
const AI_CHAT_ERROR_HTTP_METRIC: &str = "ai/chat/error/http";
const AI_CHAT_ERROR_JSON_METRIC: &str = "ai/chat/error/json";
const AI_CHAT_ERROR_URL_METRIC: &str = "ai/chat/error/url";
const AI_CHAT_ERROR_INVALID_CONFIG_METRIC: &str = "ai/chat/error/invalid_config";
const AI_CHAT_ERROR_UNEXPECTED_RESPONSE_METRIC: &str = "ai/chat/error/unexpected_response";

/// Records end-to-end latency for streaming chat requests. Unlike `ai/chat`, this metric is
/// recorded when the returned stream terminates (success or error) so it captures total stream
/// duration from `chat_stream()` invocation until termination.
const AI_CHAT_STREAM_METRIC: &str = "ai/chat_stream";
const AI_CHAT_STREAM_ERROR_TIMEOUT_METRIC: &str = "ai/chat_stream/error/timeout";
const AI_CHAT_STREAM_ERROR_CANCELLED_METRIC: &str = "ai/chat_stream/error/cancelled";
const AI_CHAT_STREAM_ERROR_HTTP_METRIC: &str = "ai/chat_stream/error/http";
const AI_CHAT_STREAM_ERROR_JSON_METRIC: &str = "ai/chat_stream/error/json";
const AI_CHAT_STREAM_ERROR_URL_METRIC: &str = "ai/chat_stream/error/url";
const AI_CHAT_STREAM_ERROR_INVALID_CONFIG_METRIC: &str = "ai/chat_stream/error/invalid_config";
const AI_CHAT_STREAM_ERROR_UNEXPECTED_RESPONSE_METRIC: &str =
    "ai/chat_stream/error/unexpected_response";

const AI_LIST_MODELS_METRIC: &str = "ai/list_models";
const AI_LIST_MODELS_RETRY_METRIC: &str = "ai/list_models/retry";
const AI_LIST_MODELS_ERROR_TIMEOUT_METRIC: &str = "ai/list_models/error/timeout";
const AI_LIST_MODELS_ERROR_CANCELLED_METRIC: &str = "ai/list_models/error/cancelled";
const AI_LIST_MODELS_ERROR_HTTP_METRIC: &str = "ai/list_models/error/http";
const AI_LIST_MODELS_ERROR_JSON_METRIC: &str = "ai/list_models/error/json";
const AI_LIST_MODELS_ERROR_URL_METRIC: &str = "ai/list_models/error/url";
const AI_LIST_MODELS_ERROR_INVALID_CONFIG_METRIC: &str = "ai/list_models/error/invalid_config";
const AI_LIST_MODELS_ERROR_UNEXPECTED_RESPONSE_METRIC: &str =
    "ai/list_models/error/unexpected_response";

fn record_chat_error_metrics(metrics: &MetricsRegistry, err: &AiError) {
    match err {
        AiError::Timeout => {
            metrics.record_timeout(AI_CHAT_METRIC);
            metrics.record_timeout(AI_CHAT_ERROR_TIMEOUT_METRIC);
        }
        // Defensive: in some call paths we may still end up with `AiError::Http(reqwest::Error)`
        // where reqwest is signalling a timeout. Treat it as a timeout for metrics so timeouts
        // are classified consistently.
        AiError::Http(err) if err.is_timeout() => {
            metrics.record_timeout(AI_CHAT_METRIC);
            metrics.record_timeout(AI_CHAT_ERROR_TIMEOUT_METRIC);
        }
        AiError::Cancelled => {
            metrics.record_error(AI_CHAT_METRIC);
            metrics.record_error(AI_CHAT_ERROR_CANCELLED_METRIC);
        }
        AiError::Http(_) => {
            metrics.record_error(AI_CHAT_METRIC);
            metrics.record_error(AI_CHAT_ERROR_HTTP_METRIC);
        }
        AiError::Json(_) => {
            metrics.record_error(AI_CHAT_METRIC);
            metrics.record_error(AI_CHAT_ERROR_JSON_METRIC);
        }
        AiError::Url(_) => {
            metrics.record_error(AI_CHAT_METRIC);
            metrics.record_error(AI_CHAT_ERROR_URL_METRIC);
        }
        AiError::InvalidConfig(_) => {
            metrics.record_error(AI_CHAT_METRIC);
            metrics.record_error(AI_CHAT_ERROR_INVALID_CONFIG_METRIC);
        }
        AiError::UnexpectedResponse(_) => {
            metrics.record_error(AI_CHAT_METRIC);
            metrics.record_error(AI_CHAT_ERROR_UNEXPECTED_RESPONSE_METRIC);
        }
    }
}

fn record_chat_stream_error_metrics(metrics: &MetricsRegistry, err: &AiError) {
    match err {
        AiError::Timeout => {
            metrics.record_timeout(AI_CHAT_STREAM_METRIC);
            metrics.record_timeout(AI_CHAT_STREAM_ERROR_TIMEOUT_METRIC);
        }
        // Defensive: streaming providers may still surface timeouts as `reqwest::Error` wrapped
        // inside `AiError::Http`. Classify these as timeouts so metrics remain consistent.
        AiError::Http(err) if err.is_timeout() => {
            metrics.record_timeout(AI_CHAT_STREAM_METRIC);
            metrics.record_timeout(AI_CHAT_STREAM_ERROR_TIMEOUT_METRIC);
        }
        AiError::Cancelled => {
            metrics.record_error(AI_CHAT_STREAM_METRIC);
            metrics.record_error(AI_CHAT_STREAM_ERROR_CANCELLED_METRIC);
        }
        AiError::Http(_) => {
            metrics.record_error(AI_CHAT_STREAM_METRIC);
            metrics.record_error(AI_CHAT_STREAM_ERROR_HTTP_METRIC);
        }
        AiError::Json(_) => {
            metrics.record_error(AI_CHAT_STREAM_METRIC);
            metrics.record_error(AI_CHAT_STREAM_ERROR_JSON_METRIC);
        }
        AiError::Url(_) => {
            metrics.record_error(AI_CHAT_STREAM_METRIC);
            metrics.record_error(AI_CHAT_STREAM_ERROR_URL_METRIC);
        }
        AiError::InvalidConfig(_) => {
            metrics.record_error(AI_CHAT_STREAM_METRIC);
            metrics.record_error(AI_CHAT_STREAM_ERROR_INVALID_CONFIG_METRIC);
        }
        AiError::UnexpectedResponse(_) => {
            metrics.record_error(AI_CHAT_STREAM_METRIC);
            metrics.record_error(AI_CHAT_STREAM_ERROR_UNEXPECTED_RESPONSE_METRIC);
        }
    }
}

fn record_list_models_error_metrics(metrics: &MetricsRegistry, err: &AiError) {
    match err {
        AiError::Timeout => {
            metrics.record_timeout(AI_LIST_MODELS_METRIC);
            metrics.record_timeout(AI_LIST_MODELS_ERROR_TIMEOUT_METRIC);
        }
        // Defensive: in some call paths we may still end up with `AiError::Http(reqwest::Error)`
        // where reqwest is signalling a timeout. Treat it as a timeout for metrics so timeouts
        // are classified consistently.
        AiError::Http(err) if err.is_timeout() => {
            metrics.record_timeout(AI_LIST_MODELS_METRIC);
            metrics.record_timeout(AI_LIST_MODELS_ERROR_TIMEOUT_METRIC);
        }
        AiError::Cancelled => {
            metrics.record_error(AI_LIST_MODELS_METRIC);
            metrics.record_error(AI_LIST_MODELS_ERROR_CANCELLED_METRIC);
        }
        AiError::Http(_) => {
            metrics.record_error(AI_LIST_MODELS_METRIC);
            metrics.record_error(AI_LIST_MODELS_ERROR_HTTP_METRIC);
        }
        AiError::Json(_) => {
            metrics.record_error(AI_LIST_MODELS_METRIC);
            metrics.record_error(AI_LIST_MODELS_ERROR_JSON_METRIC);
        }
        AiError::Url(_) => {
            metrics.record_error(AI_LIST_MODELS_METRIC);
            metrics.record_error(AI_LIST_MODELS_ERROR_URL_METRIC);
        }
        AiError::InvalidConfig(_) => {
            metrics.record_error(AI_LIST_MODELS_METRIC);
            metrics.record_error(AI_LIST_MODELS_ERROR_INVALID_CONFIG_METRIC);
        }
        AiError::UnexpectedResponse(_) => {
            metrics.record_error(AI_LIST_MODELS_METRIC);
            metrics.record_error(AI_LIST_MODELS_ERROR_UNEXPECTED_RESPONSE_METRIC);
        }
    }
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 2,
            initial_backoff: Duration::from_millis(200),
            max_backoff: Duration::from_secs(2),
        }
    }
}

#[derive(Debug)]
struct InFlightChat {
    cancel: CancellationToken,
    done: Notify,
    result: TokioMutex<Option<Result<String, AiError>>>,
}

impl InFlightChat {
    fn new() -> Self {
        Self {
            cancel: CancellationToken::new(),
            done: Notify::new(),
            result: TokioMutex::new(None),
        }
    }
}

#[derive(Debug)]
struct InFlightChatState {
    entry: Arc<InFlightChat>,
    waiters: usize,
}

pub struct AiClient {
    provider_kind: AiProviderKind,
    provider: Arc<dyn LlmProvider>,
    semaphore: Arc<Semaphore>,
    privacy: PrivacyFilter,
    default_max_tokens: u32,
    default_temperature: Option<f32>,
    request_timeout: Duration,
    audit_enabled: bool,
    provider_label: &'static str,
    model: String,
    endpoint: url::Url,
    azure_cache_key: Option<(String, String)>,
    cache: Option<Arc<LlmResponseCache>>,
    in_flight: Arc<TokioMutex<HashMap<CacheKey, InFlightChatState>>>,
    retry: RetryConfig,
}

impl AiClient {
    pub fn from_config(config: &AiConfig) -> Result<Self, AiError> {
        let concurrency = config.provider.effective_concurrency();
        if concurrency == 0 {
            return Err(AiError::InvalidConfig(
                "ai.provider.concurrency must be >= 1".into(),
            ));
        }

        let provider_kind = config.provider.kind.clone();
        if config.privacy.local_only {
            match &provider_kind {
                AiProviderKind::InProcessLlama => {}
                AiProviderKind::Ollama
                | AiProviderKind::OpenAiCompatible
                | AiProviderKind::Http => {
                    validate_local_only_url(&config.provider.url)?;
                }
                _ => {
                    return Err(AiError::InvalidConfig(format!(
                        "ai.privacy.local_only forbids cloud provider {provider_kind:?}"
                    )));
                }
            }
        }

        let timeout = config.provider.timeout();
        let mut azure_cache_key = None;

        let provider: Arc<dyn LlmProvider> = match &provider_kind {
            AiProviderKind::Ollama => Arc::new(OllamaProvider::new(
                config.provider.url.clone(),
                config.provider.model.clone(),
                timeout,
            )?),
            AiProviderKind::OpenAiCompatible => Arc::new(OpenAiCompatibleProvider::new(
                config.provider.url.clone(),
                config.provider.model.clone(),
                timeout,
                config.api_key.clone(),
            )?),
            AiProviderKind::InProcessLlama => {
                #[cfg(feature = "local-llm")]
                {
                    Arc::new(InProcessLlamaProvider::new(&config.provider)?)
                }
                #[cfg(not(feature = "local-llm"))]
                {
                    return Err(AiError::InvalidConfig(
                        "ai.provider.kind = \"in_process_llama\" requires building nova-ai with --features local-llm"
                            .into(),
                    ));
                }
            }
            AiProviderKind::OpenAi => {
                let api_key = config.api_key.clone().ok_or_else(|| {
                    AiError::InvalidConfig("OpenAI provider requires ai.api_key".into())
                })?;
                Arc::new(OpenAiCompatibleProvider::new(
                    config.provider.url.clone(),
                    config.provider.model.clone(),
                    timeout,
                    Some(api_key),
                )?)
            }
            AiProviderKind::Anthropic => {
                let api_key = config.api_key.clone().ok_or_else(|| {
                    AiError::InvalidConfig("Anthropic provider requires ai.api_key".into())
                })?;
                Arc::new(AnthropicProvider::new(
                    config.provider.url.clone(),
                    api_key,
                    config.provider.model.clone(),
                    timeout,
                )?)
            }
            AiProviderKind::Gemini => {
                let api_key = config.api_key.clone().ok_or_else(|| {
                    AiError::InvalidConfig("Gemini provider requires ai.api_key".into())
                })?;
                Arc::new(GeminiProvider::new(
                    config.provider.url.clone(),
                    api_key,
                    config.provider.model.clone(),
                    timeout,
                )?)
            }
            AiProviderKind::AzureOpenAi => {
                let api_key = config.api_key.clone().ok_or_else(|| {
                    AiError::InvalidConfig("Azure OpenAI provider requires ai.api_key".into())
                })?;
                let deployment = config.provider.azure_deployment.clone().ok_or_else(|| {
                    AiError::InvalidConfig(
                        "Azure OpenAI provider requires ai.provider.azure_deployment".into(),
                    )
                })?;
                let api_version = config
                    .provider
                    .azure_api_version
                    .clone()
                    .unwrap_or_else(|| "2024-02-01".to_string());

                azure_cache_key = Some((deployment.clone(), api_version.clone()));
                Arc::new(AzureOpenAiProvider::new(
                    config.provider.url.clone(),
                    api_key,
                    deployment,
                    api_version,
                    timeout,
                )?)
            }
            AiProviderKind::Http => Arc::new(HttpProvider::new(
                config.provider.url.clone(),
                config.api_key.clone(),
                config.provider.model.clone(),
                timeout,
            )?),
        };

        let (model, endpoint) = match &provider_kind {
            AiProviderKind::InProcessLlama => {
                let Some(in_process) = config.provider.in_process_llama.as_ref() else {
                    return Err(AiError::InvalidConfig(
                        "ai.provider.in_process_llama must be set when kind = \"in_process_llama\""
                            .into(),
                    ));
                };
                let model = in_process
                    .model_path
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_else(|| config.provider.model.clone());
                (model, in_process_endpoint_id(in_process)?)
            }
            _ => (config.provider.model.clone(), config.provider.url.clone()),
        };

        let cache = if config.cache_enabled {
            if config.cache_max_entries == 0 {
                return Err(AiError::InvalidConfig(
                    "ai.cache_max_entries must be >= 1".into(),
                ));
            }
            if config.cache_ttl_secs == 0 {
                return Err(AiError::InvalidConfig(
                    "ai.cache_ttl_secs must be > 0".into(),
                ));
            }

            Some(shared_cache(CacheSettings {
                max_entries: config.cache_max_entries,
                ttl: Duration::from_secs(config.cache_ttl_secs),
            }))
        } else {
            None
        };

        Ok(Self {
            provider_kind,
            provider,
            semaphore: Arc::new(Semaphore::new(concurrency)),
            privacy: PrivacyFilter::new(&config.privacy)?,
            default_max_tokens: config.provider.max_tokens,
            default_temperature: config.provider.temperature,
            request_timeout: config.provider.timeout(),
            audit_enabled: config.enabled && config.audit_log.enabled,
            provider_label: provider_label(&config.provider.kind),
            model,
            endpoint,
            azure_cache_key,
            cache,
            in_flight: Arc::new(TokioMutex::new(HashMap::new())),
            retry: RetryConfig {
                max_retries: config.provider.retry_max_retries,
                initial_backoff: Duration::from_millis(config.provider.retry_initial_backoff_ms),
                max_backoff: Duration::from_millis(config.provider.retry_max_backoff_ms),
            },
        })
    }

    pub fn sanitize_snippet(&self, snippet: &CodeSnippet) -> Option<String> {
        let mut session = self.privacy.new_session();
        self.privacy.sanitize_snippet(&mut session, snippet)
    }

    pub fn is_excluded_path(&self, path: &Path) -> bool {
        self.privacy.is_excluded(path)
    }

    pub async fn chat(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<String, AiError> {
        <Self as LlmClient>::chat(self, request, cancel).await
    }

    pub async fn chat_stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<AiStream, AiError> {
        <Self as LlmClient>::chat_stream(self, request, cancel).await
    }

    pub async fn list_models(&self, cancel: CancellationToken) -> Result<Vec<String>, AiError> {
        <Self as LlmClient>::list_models(self, cancel).await
    }

    async fn acquire_permit(
        &self,
        cancel: &CancellationToken,
    ) -> Result<tokio::sync::OwnedSemaphorePermit, AiError> {
        tokio::select! {
            _ = cancel.cancelled() => Err(AiError::Cancelled),
            permit = self.semaphore.clone().acquire_owned() => permit
                .map_err(|_| AiError::UnexpectedResponse("ai client shutting down".into())),
        }
    }

    async fn cancel_in_flight_waiter(&self, key: CacheKey, entry: &Arc<InFlightChat>) {
        let mut in_flight = self.in_flight.lock().await;
        let Some(state) = in_flight.get_mut(&key) else {
            return;
        };

        if !Arc::ptr_eq(&state.entry, entry) {
            return;
        }

        state.waiters = state.waiters.saturating_sub(1);
        if state.waiters == 0 {
            in_flight.remove(&key);
            entry.cancel.cancel();
        }
    }

    fn sanitize_request(&self, mut request: ChatRequest) -> ChatRequest {
        if request.max_tokens.is_none() {
            request.max_tokens = Some(self.default_max_tokens);
        }
        if request.temperature.is_none() {
            request.temperature = self.default_temperature;
        }

        let mut session = self.privacy.new_session();
        for message in &mut request.messages {
            let sanitized = self
                .privacy
                .sanitize_prompt_text(&mut session, &message.content);
            message.content = sanitized;
        }

        request
    }

    fn build_cache_key(&self, request: &ChatRequest) -> CacheKey {
        let mut builder = CacheKeyBuilder::new("ai_chat_v1");
        builder.push_str(self.provider_label);
        if let Some((deployment, api_version)) = &self.azure_cache_key {
            builder.push_str(deployment);
            builder.push_str(api_version);
        }
        builder.push_str(self.endpoint.as_str());
        builder.push_str(&self.model);
        builder.push_u32(request.max_tokens.unwrap_or(self.default_max_tokens));
        // Include the option discriminant so `temperature: None` doesn't collide with
        // `temperature: Some(0.0)` (whose IEEE-754 bits are also zero).
        match request.temperature {
            Some(temp) => {
                builder.push_u32(1);
                builder.push_u32(temp.to_bits());
            }
            None => builder.push_u32(0),
        }
        builder.push_u64(
            request
                .messages
                .len()
                .try_into()
                .expect("message count should fit u64"),
        );
        for message in &request.messages {
            let role = match message.role {
                crate::types::ChatRole::System => "system",
                crate::types::ChatRole::User => "user",
                crate::types::ChatRole::Assistant => "assistant",
            };
            builder.push_str(role);
            builder.push_str(&message.content);
        }
        builder.finish()
    }

    fn should_retry(&self, err: &AiError) -> bool {
        match err {
            AiError::Cancelled => false,
            AiError::Timeout => true,
            AiError::Http(err) => {
                if err.is_timeout() || err.is_connect() {
                    return true;
                }
                let Some(status) = err.status() else {
                    // Network errors without a status are generally worth retrying.
                    return true;
                };
                status.as_u16() == 408 || status.as_u16() == 429 || status.is_server_error()
            }
            _ => false,
        }
    }

    async fn backoff_sleep(
        &self,
        attempt: usize,
        max_delay: Duration,
        cancel: &CancellationToken,
    ) -> Result<(), AiError> {
        let factor = 2u32.saturating_pow((attempt.saturating_sub(1)).min(16) as u32);
        let mut delay = self.retry.initial_backoff.saturating_mul(factor);
        if delay > self.retry.max_backoff {
            delay = self.retry.max_backoff;
        }
        if delay > max_delay {
            delay = max_delay;
        }

        tokio::select! {
            _ = cancel.cancelled() => Err(AiError::Cancelled),
            _ = tokio::time::sleep(delay) => Ok(()),
        }
    }
}

#[async_trait]
impl LlmClient for AiClient {
    async fn chat(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<String, AiError> {
        let metrics = MetricsRegistry::global();
        let metrics_start = Instant::now();

        let result: Result<String, AiError> = 'chat_result: {
            if cancel.is_cancelled() {
                break 'chat_result Err(AiError::Cancelled);
            }

            let request = self.sanitize_request(request);

            let prompt_for_log = if self.audit_enabled {
                Some(audit::format_chat_prompt(&request.messages))
            } else {
                None
            };
            let request_id = if self.audit_enabled {
                audit::next_request_id()
            } else {
                0
            };
            let safe_endpoint = if self.audit_enabled {
                Some(audit::sanitize_url_for_log(&self.endpoint))
            } else {
                None
            };

            if let Some(cache) = &self.cache {
                let key = self.build_cache_key(&request);
                if let Some(hit) = cache.get(key).await {
                    if let Some(prompt) = prompt_for_log.as_deref() {
                        let started_at = Instant::now();
                        audit::log_llm_request(
                            request_id,
                            self.provider_label,
                            &self.model,
                            prompt,
                            safe_endpoint.as_deref(),
                            /*attempt=*/ 0,
                            /*stream=*/ false,
                        );
                        audit::log_llm_response(
                            request_id,
                            self.provider_label,
                            &self.model,
                            safe_endpoint.as_deref(),
                            &hit,
                            started_at.elapsed(),
                            /*retry_count=*/ 0,
                            /*stream=*/ false,
                            /*chunk_count=*/ None,
                        );
                    } else {
                        debug!(
                            provider = self.provider_label,
                            model = %self.model,
                            "llm cache hit"
                        );
                    }

                    metrics.record_request(AI_CHAT_CACHE_HIT_METRIC, Duration::from_micros(1));
                    break 'chat_result Ok(hit);
                }

                metrics.record_request(AI_CHAT_CACHE_MISS_METRIC, Duration::from_micros(1));

                let (entry, is_leader) = {
                    let mut in_flight = self.in_flight.lock().await;
                    if let Some(state) = in_flight.get_mut(&key) {
                        state.waiters = state.waiters.saturating_add(1);
                        metrics.record_request(
                            AI_CHAT_COALESCED_WAITER_METRIC,
                            Duration::from_micros(1),
                        );
                        (state.entry.clone(), false)
                    } else {
                        let entry = Arc::new(InFlightChat::new());
                        in_flight.insert(
                            key,
                            InFlightChatState {
                                entry: entry.clone(),
                                waiters: 1,
                            },
                        );
                        (entry, true)
                    }
                };

                if is_leader {
                    let provider_kind = self.provider_kind.clone();
                    let provider = self.provider.clone();
                    let semaphore = self.semaphore.clone();
                    let provider_label = self.provider_label;
                    let model = self.model.clone();
                    let request = request.clone();
                    let timeout = self.request_timeout;
                    let retry = self.retry.clone();
                    let prompt_for_log = prompt_for_log.clone();
                    let request_id = request_id;
                    let safe_endpoint = safe_endpoint.clone();
                    let cache = cache.clone();
                    let in_flight = self.in_flight.clone();
                    let entry_for_task = entry.clone();

                    tokio::spawn(async move {
                        fn should_retry(err: &AiError) -> bool {
                            match err {
                                AiError::Cancelled => false,
                                AiError::Timeout => true,
                                AiError::Http(err) => {
                                    if err.is_timeout() || err.is_connect() {
                                        return true;
                                    }
                                    let Some(status) = err.status() else {
                                        // Network errors without a status are generally worth retrying.
                                        return true;
                                    };
                                    status.as_u16() == 408
                                        || status.as_u16() == 429
                                        || status.is_server_error()
                                }
                                _ => false,
                            }
                        }

                        async fn acquire_permit(
                            semaphore: &Arc<Semaphore>,
                            cancel: &CancellationToken,
                        ) -> Result<tokio::sync::OwnedSemaphorePermit, AiError> {
                            tokio::select! {
                                _ = cancel.cancelled() => Err(AiError::Cancelled),
                                permit = semaphore.clone().acquire_owned() => permit
                                    .map_err(|_| AiError::UnexpectedResponse("ai client shutting down".into())),
                            }
                        }

                        async fn backoff_sleep(
                            retry: &RetryConfig,
                            attempt: usize,
                            max_delay: Duration,
                            cancel: &CancellationToken,
                        ) -> Result<(), AiError> {
                            let factor =
                                2u32.saturating_pow((attempt.saturating_sub(1)).min(16) as u32);
                            let mut delay = retry.initial_backoff.saturating_mul(factor);
                            if delay > retry.max_backoff {
                                delay = retry.max_backoff;
                            }
                            if delay > max_delay {
                                delay = max_delay;
                            }

                            tokio::select! {
                                _ = cancel.cancelled() => Err(AiError::Cancelled),
                                _ = tokio::time::sleep(delay) => Ok(()),
                            }
                        }

                        let metrics = MetricsRegistry::global();
                        let operation_start = Instant::now();
                        let mut attempt = 0usize;
                        let cancel = entry_for_task.cancel.clone();

                        let result: Result<String, AiError> = 'provider_result: loop {
                            if cancel.is_cancelled() {
                                break 'provider_result Err(AiError::Cancelled);
                            }

                            let remaining = timeout.saturating_sub(operation_start.elapsed());
                            if remaining == Duration::ZERO {
                                break 'provider_result Err(AiError::Timeout);
                            }

                            if attempt > 0 {
                                metrics.record_request(
                                    AI_CHAT_RETRY_METRIC,
                                    Duration::from_micros(1),
                                );
                            }

                            let (started_at, result) = {
                                let permit = match tokio::time::timeout(
                                    remaining,
                                    acquire_permit(&semaphore, &cancel),
                                )
                                .await
                                {
                                    Ok(permit) => match permit {
                                        Ok(permit) => permit,
                                        Err(err) => break 'provider_result Err(err),
                                    },
                                    Err(_) => break 'provider_result Err(AiError::Timeout),
                                };

                                let remaining = timeout.saturating_sub(operation_start.elapsed());
                                if remaining == Duration::ZERO {
                                    drop(permit);
                                    break 'provider_result Err(AiError::Timeout);
                                }

                                let started_at = Instant::now();
                                if let Some(prompt) = prompt_for_log.as_deref() {
                                    audit::log_llm_request(
                                        request_id,
                                        provider_label,
                                        &model,
                                        prompt,
                                        safe_endpoint.as_deref(),
                                        attempt,
                                        /*stream=*/ false,
                                    );
                                } else {
                                    debug!(
                                        provider = provider_label,
                                        model = %model,
                                        attempt,
                                        "llm request"
                                    );
                                }

                                let out = match tokio::time::timeout(
                                    remaining,
                                    provider.chat(request.clone(), cancel.clone()),
                                )
                                .await
                                {
                                    Ok(res) => res,
                                    Err(_) => Err(AiError::Timeout),
                                };

                                drop(permit);
                                (started_at, out)
                            };

                            match result {
                                Ok(completion) => {
                                    if prompt_for_log.is_some() {
                                        audit::log_llm_response(
                                            request_id,
                                            provider_label,
                                            &model,
                                            safe_endpoint.as_deref(),
                                            &completion,
                                            started_at.elapsed(),
                                            /*retry_count=*/ attempt,
                                            /*stream=*/ false,
                                            /*chunk_count=*/ None,
                                        );
                                    }
                                    break 'provider_result Ok(completion);
                                }
                                Err(err)
                                    if attempt < retry.max_retries && should_retry(&err) =>
                                {
                                    if prompt_for_log.is_some() {
                                        audit::log_llm_error(
                                            request_id,
                                            provider_label,
                                            &model,
                                            &err.to_string(),
                                            started_at.elapsed(),
                                            /*retry_count=*/ attempt,
                                            /*stream=*/ false,
                                        );
                                    }

                                    attempt += 1;
                                    warn!(
                                        provider = ?provider_kind,
                                        attempt,
                                        error = %err,
                                        "llm request failed, retrying"
                                    );

                                    let remaining = timeout
                                        .saturating_sub(operation_start.elapsed());
                                    if remaining == Duration::ZERO {
                                        break 'provider_result Err(AiError::Timeout);
                                    }

                                    if let Err(err) =
                                        backoff_sleep(&retry, attempt, remaining, &cancel).await
                                    {
                                        if prompt_for_log.is_some() {
                                            audit::log_llm_error(
                                                request_id,
                                                provider_label,
                                                &model,
                                                &err.to_string(),
                                                operation_start.elapsed(),
                                                /*retry_count=*/ attempt,
                                                /*stream=*/ false,
                                            );
                                        }
                                        break 'provider_result Err(err);
                                    }
                                }
                                Err(err) => {
                                    if prompt_for_log.is_some() {
                                        audit::log_llm_error(
                                            request_id,
                                            provider_label,
                                            &model,
                                            &err.to_string(),
                                            started_at.elapsed(),
                                            /*retry_count=*/ attempt,
                                            /*stream=*/ false,
                                        );
                                    }
                                    break 'provider_result Err(err);
                                }
                            }
                        };

                        let completion_for_cache = result.as_ref().ok().cloned();
                        {
                            let mut guard = entry_for_task.result.lock().await;
                            *guard = Some(result);
                        }
                        entry_for_task.done.notify_waiters();

                        if let Some(completion) = completion_for_cache {
                            cache.insert(key, completion).await;
                        }

                        let mut in_flight = in_flight.lock().await;
                        if let Some(state) = in_flight.get(&key) {
                            if Arc::ptr_eq(&state.entry, &entry_for_task) {
                                in_flight.remove(&key);
                            }
                        }
                    });
                }

                loop {
                    let notified = entry.done.notified();
                    if let Some(result) = entry.result.lock().await.clone() {
                        break 'chat_result result;
                    }

                    tokio::select! {
                        _ = cancel.cancelled() => {
                            self.cancel_in_flight_waiter(key, &entry).await;
                            break 'chat_result Err(AiError::Cancelled);
                        }
                        _ = notified => {}
                    }
                }
            }

            let timeout = self.request_timeout;
            let operation_start = Instant::now();
            let mut attempt = 0usize;

            loop {
                if cancel.is_cancelled() {
                    break 'chat_result Err(AiError::Cancelled);
                }

                let remaining = timeout.saturating_sub(operation_start.elapsed());
                if remaining == Duration::ZERO {
                    break 'chat_result Err(AiError::Timeout);
                }

                if attempt > 0 {
                    metrics.record_request(AI_CHAT_RETRY_METRIC, Duration::from_micros(1));
                }

                let (started_at, result) = {
                    let permit =
                        match tokio::time::timeout(remaining, self.acquire_permit(&cancel)).await {
                            Ok(permit) => match permit {
                                Ok(permit) => permit,
                                Err(err) => break 'chat_result Err(err),
                            },
                            Err(_) => break 'chat_result Err(AiError::Timeout),
                        };

                    let remaining = timeout.saturating_sub(operation_start.elapsed());
                    if remaining == Duration::ZERO {
                        drop(permit);
                        break 'chat_result Err(AiError::Timeout);
                    }

                    let started_at = Instant::now();
                    if let Some(prompt) = prompt_for_log.as_deref() {
                        audit::log_llm_request(
                            request_id,
                            self.provider_label,
                            &self.model,
                            prompt,
                            safe_endpoint.as_deref(),
                            attempt,
                            /*stream=*/ false,
                        );
                    } else {
                        debug!(
                            provider = self.provider_label,
                            model = %self.model,
                            attempt,
                            "llm request"
                        );
                    }

                    let out = match tokio::time::timeout(
                        remaining,
                        self.provider.chat(request.clone(), cancel.clone()),
                    )
                    .await
                    {
                        Ok(res) => res,
                        Err(_) => Err(AiError::Timeout),
                    };

                    drop(permit);
                    (started_at, out)
                };

                match result {
                    Ok(completion) => {
                        if self.audit_enabled {
                            audit::log_llm_response(
                                request_id,
                                self.provider_label,
                                &self.model,
                                safe_endpoint.as_deref(),
                                &completion,
                                started_at.elapsed(),
                                /*retry_count=*/ attempt,
                                /*stream=*/ false,
                                /*chunk_count=*/ None,
                            );
                        }
                        break 'chat_result Ok(completion);
                    }
                    Err(err) if attempt < self.retry.max_retries && self.should_retry(&err) => {
                        if self.audit_enabled {
                            audit::log_llm_error(
                                request_id,
                                self.provider_label,
                                &self.model,
                                &err.to_string(),
                                started_at.elapsed(),
                                /*retry_count=*/ attempt,
                                /*stream=*/ false,
                            );
                        }

                        attempt += 1;
                        warn!(
                            provider = ?self.provider_kind,
                            attempt,
                            error = %err,
                            "llm request failed, retrying"
                        );

                        let remaining = timeout.saturating_sub(operation_start.elapsed());
                        if remaining == Duration::ZERO {
                            break 'chat_result Err(AiError::Timeout);
                        }
                        if let Err(err) = self.backoff_sleep(attempt, remaining, &cancel).await {
                            if self.audit_enabled {
                                audit::log_llm_error(
                                    request_id,
                                    self.provider_label,
                                    &self.model,
                                    &err.to_string(),
                                    operation_start.elapsed(),
                                    /*retry_count=*/ attempt,
                                    /*stream=*/ false,
                                );
                            }
                            break 'chat_result Err(err);
                        }
                    }
                    Err(err) => {
                        if self.audit_enabled {
                            audit::log_llm_error(
                                request_id,
                                self.provider_label,
                                &self.model,
                                &err.to_string(),
                                started_at.elapsed(),
                                /*retry_count=*/ attempt,
                                /*stream=*/ false,
                            );
                        }
                        break 'chat_result Err(err);
                    }
                }
            }
        };

        metrics.record_request(AI_CHAT_METRIC, metrics_start.elapsed());
        if let Err(err) = &result {
            record_chat_error_metrics(metrics, err);
        }

        result
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<AiStream, AiError> {
        let metrics = MetricsRegistry::global();
        let metrics_start = Instant::now();
        let record_failure = |err: &AiError| {
            metrics.record_request(AI_CHAT_STREAM_METRIC, metrics_start.elapsed());
            record_chat_stream_error_metrics(metrics, err);
        };

        if cancel.is_cancelled() {
            let err = AiError::Cancelled;
            record_failure(&err);
            return Err(err);
        }

        let request = self.sanitize_request(request);

        let prompt_for_log = if self.audit_enabled {
            Some(audit::format_chat_prompt(&request.messages))
        } else {
            None
        };
        let request_id = if self.audit_enabled {
            audit::next_request_id()
        } else {
            0
        };
        let safe_endpoint = if self.audit_enabled {
            Some(audit::sanitize_url_for_log(&self.endpoint))
        } else {
            None
        };

        let cache_key = self.cache.as_ref().map(|_| self.build_cache_key(&request));
        if let (Some(cache), Some(key)) = (&self.cache, cache_key) {
            match cache.get(key).await {
                Some(hit) => {
                    if let Some(prompt) = prompt_for_log.as_deref() {
                        let started_at = Instant::now();
                        audit::log_llm_request(
                            request_id,
                            self.provider_label,
                            &self.model,
                            prompt,
                            safe_endpoint.as_deref(),
                            /*attempt=*/ 0,
                            /*stream=*/ true,
                        );
                        audit::log_llm_response(
                            request_id,
                            self.provider_label,
                            &self.model,
                            safe_endpoint.as_deref(),
                            &hit,
                            started_at.elapsed(),
                            /*retry_count=*/ 0,
                            /*stream=*/ true,
                            /*chunk_count=*/ Some(1),
                        );
                    } else {
                        debug!(
                            provider = self.provider_label,
                            model = %self.model,
                            "llm cache hit"
                        );
                    }

                    metrics.record_request(AI_CHAT_CACHE_HIT_METRIC, Duration::from_micros(1));

                    let metrics_start_for_stream = metrics_start;
                    let stream = async_stream::try_stream! {
                        yield hit;
                        metrics.record_request(
                            AI_CHAT_STREAM_METRIC,
                            metrics_start_for_stream.elapsed(),
                        );
                    };
                    return Ok(Box::pin(stream));
                }
                None => {
                    metrics.record_request(AI_CHAT_CACHE_MISS_METRIC, Duration::from_micros(1));
                }
            }
        }

        let timeout = self.request_timeout;
        let operation_start = Instant::now();
        // Acquire a semaphore permit per attempt. This mirrors `chat()` and avoids holding the
        // concurrency slot while we back off between retries. The permit from the successful
        // attempt is held for the lifetime of the returned stream.
        let mut attempt = 0usize;

        let (permit, inner, started_at, retry_count) = loop {
            if cancel.is_cancelled() {
                let err = AiError::Cancelled;
                record_failure(&err);
                return Err(err);
            }

            let remaining = timeout.saturating_sub(operation_start.elapsed());
            if remaining == Duration::ZERO {
                let err = AiError::Timeout;
                record_failure(&err);
                return Err(err);
            }

            let permit = match tokio::time::timeout(remaining, self.acquire_permit(&cancel)).await
            {
                Ok(result) => match result {
                    Ok(permit) => permit,
                    Err(err) => {
                        record_failure(&err);
                        return Err(err);
                    }
                },
                Err(_) => {
                    let err = AiError::Timeout;
                    record_failure(&err);
                    return Err(err);
                }
            };

            let remaining = timeout.saturating_sub(operation_start.elapsed());
            if remaining == Duration::ZERO {
                drop(permit);
                let err = AiError::Timeout;
                record_failure(&err);
                return Err(err);
            }

            let started_at = Instant::now();
            if let Some(prompt) = prompt_for_log.as_deref() {
                audit::log_llm_request(
                    request_id,
                    self.provider_label,
                    &self.model,
                    prompt,
                    safe_endpoint.as_deref(),
                    attempt,
                    /*stream=*/ true,
                );
            }

            let out = match tokio::time::timeout(
                remaining,
                self.provider.chat_stream(request.clone(), cancel.clone()),
            )
            .await
            {
                Ok(res) => res,
                Err(_) => Err(AiError::Timeout),
            };

            match out {
                Ok(stream) => break (permit, stream, started_at, attempt),
                Err(err) if attempt < self.retry.max_retries && self.should_retry(&err) => {
                    if self.audit_enabled {
                        audit::log_llm_error(
                            request_id,
                            self.provider_label,
                            &self.model,
                            &err.to_string(),
                            started_at.elapsed(),
                            attempt,
                            /*stream=*/ true,
                        );
                    }

                    attempt += 1;
                    warn!(
                        provider = ?self.provider_kind,
                        attempt,
                        error = %err,
                        "llm stream request failed, retrying"
                    );

                    drop(permit);
                    let remaining = timeout.saturating_sub(operation_start.elapsed());
                    if remaining == Duration::ZERO {
                        let err = AiError::Timeout;
                        record_failure(&err);
                        return Err(err);
                    }
                    if let Err(err) = self.backoff_sleep(attempt, remaining, &cancel).await {
                        if self.audit_enabled {
                            audit::log_llm_error(
                                request_id,
                                self.provider_label,
                                &self.model,
                                &err.to_string(),
                                operation_start.elapsed(),
                                attempt,
                                /*stream=*/ true,
                            );
                        }
                        record_failure(&err);
                        return Err(err);
                    }
                }
                Err(err) => {
                    if self.audit_enabled {
                        audit::log_llm_error(
                            request_id,
                            self.provider_label,
                            &self.model,
                            &err.to_string(),
                            started_at.elapsed(),
                            attempt,
                            /*stream=*/ true,
                        );
                    }
                    drop(permit);
                    record_failure(&err);
                    return Err(err);
                }
            }
        };

        let audit_enabled = self.audit_enabled;
        let provider_label = self.provider_label;
        let model = self.model.clone();
        let safe_endpoint_for_stream = safe_endpoint.clone();
        let request_id_for_stream = request_id;
        let started_at_for_stream = started_at;
        let retry_count_for_stream = retry_count;
        let idle_timeout = self.request_timeout;

        // Clone before moving `cancel_for_stream` into the wrapper so it can enforce cancellation
        // even if the provider's stream doesn't poll the token correctly.
        let cancel_for_stream = cancel.clone();
        let cache_for_stream = self.cache.clone();
        let cache_key_for_stream = cache_key;
        let metrics_start_for_stream = metrics_start;

        let stream = async_stream::try_stream! {
            let _permit = permit;
            let mut inner = inner;
            let mut completion = String::new();
            let mut chunk_count = 0usize;
            let stream_result: Result<(), AiError> = loop {
                let item = tokio::select! {
                    biased;
                    _ = cancel_for_stream.cancelled() => Err(AiError::Cancelled),
                    item = tokio::time::timeout(idle_timeout, inner.next()) => match item {
                        Ok(item) => Ok(item),
                        Err(_) => Err(AiError::Timeout),
                    },
                };

                match item {
                    Ok(Some(Ok(chunk))) => {
                        chunk_count += 1;
                        completion.push_str(&chunk);
                        yield chunk;
                    }
                    Ok(Some(Err(err))) => {
                        if audit_enabled {
                            audit::log_llm_error(
                                request_id_for_stream,
                                provider_label,
                                &model,
                                &err.to_string(),
                                started_at_for_stream.elapsed(),
                                retry_count_for_stream,
                                /*stream=*/ true,
                            );
                        }
                        break Err(err);
                    }
                    Ok(None) => break Ok(()),
                    Err(err) => {
                        if audit_enabled {
                            audit::log_llm_error(
                                request_id_for_stream,
                                provider_label,
                                &model,
                                &err.to_string(),
                                started_at_for_stream.elapsed(),
                                retry_count_for_stream,
                                /*stream=*/ true,
                            );
                        }
                        break Err(err);
                    }
                }
            };

            if stream_result.is_ok() {
                if let (Some(cache), Some(key)) =
                    (cache_for_stream.as_ref(), cache_key_for_stream)
                {
                    cache.insert(key, completion.clone()).await;
                }

                if audit_enabled {
                    audit::log_llm_response(
                        request_id_for_stream,
                        provider_label,
                        &model,
                        safe_endpoint_for_stream.as_deref(),
                        &completion,
                        started_at_for_stream.elapsed(),
                        retry_count_for_stream,
                        /*stream=*/ true,
                        Some(chunk_count),
                    );
                }
            }

            metrics.record_request(AI_CHAT_STREAM_METRIC, metrics_start_for_stream.elapsed());
            if let Err(err) = &stream_result {
                record_chat_stream_error_metrics(metrics, err);
            }
            stream_result?;
        };

        Ok(Box::pin(stream))
    }

    async fn list_models(&self, cancel: CancellationToken) -> Result<Vec<String>, AiError> {
        let metrics = MetricsRegistry::global();
        let metrics_start = Instant::now();

        let result: Result<Vec<String>, AiError> = 'list_models_result: {
            if cancel.is_cancelled() {
                break 'list_models_result Err(AiError::Cancelled);
            }

            let timeout = self.request_timeout;
            let operation_start = Instant::now();
            let mut attempt = 0usize;

            loop {
                if cancel.is_cancelled() {
                    break 'list_models_result Err(AiError::Cancelled);
                }

                let remaining = timeout.saturating_sub(operation_start.elapsed());
                if remaining == Duration::ZERO {
                    break 'list_models_result Err(AiError::Timeout);
                }

                if attempt > 0 {
                    metrics.record_request(AI_LIST_MODELS_RETRY_METRIC, Duration::from_micros(1));
                }

                let (result, should_backoff) = {
                    let permit = tokio::time::timeout(remaining, self.acquire_permit(&cancel))
                        .await
                        .map_err(|_| AiError::Timeout)??;

                    let remaining = timeout.saturating_sub(operation_start.elapsed());
                    if remaining == Duration::ZERO {
                        drop(permit);
                        break 'list_models_result Err(AiError::Timeout);
                    }

                    let out =
                        tokio::time::timeout(remaining, self.provider.list_models(cancel.clone()))
                            .await;
                    let out = match out {
                        Ok(res) => res,
                        Err(_) => Err(AiError::Timeout),
                    };
                    drop(permit);

                    let should_backoff = out.as_ref().is_err_and(|err| {
                        attempt < self.retry.max_retries && self.should_retry(err)
                    });
                    (out, should_backoff)
                };

                match result {
                    Ok(models) => break 'list_models_result Ok(models),
                    Err(err) if should_backoff => {
                        attempt += 1;
                        warn!(
                            provider = ?self.provider_kind,
                            attempt,
                            "llm list_models failed, retrying"
                        );

                        let remaining = timeout.saturating_sub(operation_start.elapsed());
                        if remaining == Duration::ZERO {
                            break 'list_models_result Err(AiError::Timeout);
                        }
                        if let Err(err) = self.backoff_sleep(attempt, remaining, &cancel).await {
                            break 'list_models_result Err(err);
                        }
                    }
                    Err(err) => break 'list_models_result Err(err),
                }
            }
        };

        metrics.record_request(AI_LIST_MODELS_METRIC, metrics_start.elapsed());
        if let Err(err) = &result {
            record_list_models_error_metrics(metrics, err);
        }

        result
    }
}

pub(crate) fn validate_local_only_url(url: &url::Url) -> Result<(), AiError> {
    let is_loopback = match url.host() {
        Some(Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    };

    if is_loopback {
        return Ok(());
    }

    Err(AiError::InvalidConfig(format!(
        "ai.privacy.local_only=true requires ai.provider.url to use a loopback host \
        (localhost/127.0.0.1/[::1]); got {url}"
    )))
}

fn provider_label(kind: &AiProviderKind) -> &'static str {
    match kind {
        AiProviderKind::Ollama => "ollama",
        AiProviderKind::OpenAiCompatible => "openai_compatible",
        AiProviderKind::InProcessLlama => "in_process_llama",
        AiProviderKind::OpenAi => "openai",
        AiProviderKind::Anthropic => "anthropic",
        AiProviderKind::Gemini => "gemini",
        AiProviderKind::AzureOpenAi => "azure_openai",
        AiProviderKind::Http => "http",
    }
}

fn in_process_endpoint_id(cfg: &nova_config::InProcessLlamaConfig) -> Result<url::Url, AiError> {
    use std::fmt::Write as _;

    let mut hasher = Sha256::new();
    hasher.update(cfg.model_path.to_string_lossy().as_bytes());
    hasher.update(b"\0");
    hasher.update(
        u64::try_from(cfg.context_size)
            .unwrap_or(u64::MAX)
            .to_le_bytes(),
    );
    hasher.update(cfg.temperature.to_le_bytes());
    hasher.update(cfg.top_p.to_le_bytes());
    hasher.update(cfg.gpu_layers.to_le_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(16);
    for byte in digest.iter().take(8).copied() {
        write!(&mut hex, "{byte:02x}").expect("writing to string should not fail");
    }

    url::Url::parse(&format!("inprocess://local/{hex}"))
        .map_err(|err| AiError::InvalidConfig(format!("invalid in-process endpoint id: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use futures::TryStreamExt;
    use nova_config::AiPrivacyConfig;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tracing::{field::Visit, Event};
    use tracing_subscriber::{layer::Context, prelude::*, Layer};

    #[derive(Debug, Clone)]
    struct CapturedEvent {
        target: String,
        fields: HashMap<String, String>,
    }

    #[derive(Clone)]
    struct CapturingLayer {
        events: Arc<Mutex<Vec<CapturedEvent>>>,
    }

    impl<S> Layer<S> for CapturingLayer
    where
        S: tracing::Subscriber,
    {
        fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
            let mut visitor = FieldVisitor::default();
            event.record(&mut visitor);

            let captured = CapturedEvent {
                target: event.metadata().target().to_string(),
                fields: visitor.fields,
            };
            self.events
                .lock()
                .expect("events mutex poisoned")
                .push(captured);
        }
    }

    #[derive(Default)]
    struct FieldVisitor {
        fields: HashMap<String, String>,
    }

    impl Visit for FieldVisitor {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.fields
                .insert(field.name().to_string(), format!("{value:?}"));
        }

        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.fields
                .insert(field.name().to_string(), value.to_string());
        }

        fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
            self.fields
                .insert(field.name().to_string(), value.to_string());
        }

        fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
            self.fields
                .insert(field.name().to_string(), value.to_string());
        }

        fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
            self.fields
                .insert(field.name().to_string(), value.to_string());
        }
    }

    #[derive(Clone, Default)]
    struct DummyProvider;

    const SECRET: &str = "sk-proj-012345678901234567890123456789";

    #[async_trait]
    impl LlmProvider for DummyProvider {
        async fn chat(
            &self,
            request: ChatRequest,
            _cancel: CancellationToken,
        ) -> Result<String, AiError> {
            let prompt = audit::format_chat_prompt(&request.messages);
            assert!(
                prompt.contains(SECRET),
                "expected provider to receive unmodified prompt content"
            );
            assert!(
                !prompt.contains("[REDACTED]"),
                "audit logging must not mutate provider-visible prompts"
            );
            Ok(format!("completion {SECRET}"))
        }

        async fn chat_stream(
            &self,
            request: ChatRequest,
            _cancel: CancellationToken,
        ) -> Result<AiStream, AiError> {
            let prompt = audit::format_chat_prompt(&request.messages);
            assert!(
                prompt.contains(SECRET),
                "expected provider to receive unmodified prompt content"
            );
            assert!(
                !prompt.contains("[REDACTED]"),
                "audit logging must not mutate provider-visible prompts"
            );

            let stream = async_stream::try_stream! {
                yield "chunk ".to_string();
                yield SECRET.to_string();
            };
            Ok(Box::pin(stream))
        }

        async fn list_models(&self, _cancel: CancellationToken) -> Result<Vec<String>, AiError> {
            Ok(vec!["dummy".to_string()])
        }
    }

    #[derive(Clone, Default)]
    struct NeverYieldingProvider;

    #[async_trait]
    impl LlmProvider for NeverYieldingProvider {
        async fn chat(
            &self,
            _request: ChatRequest,
            _cancel: CancellationToken,
        ) -> Result<String, AiError> {
            Err(AiError::UnexpectedResponse(
                "NeverYieldingProvider does not support chat".to_string(),
            ))
        }

        async fn chat_stream(
            &self,
            _request: ChatRequest,
            _cancel: CancellationToken,
        ) -> Result<AiStream, AiError> {
            // A stream that never yields and never terminates.
            Ok(Box::pin(
                futures::stream::pending::<Result<String, AiError>>(),
            ))
        }

        async fn list_models(&self, _cancel: CancellationToken) -> Result<Vec<String>, AiError> {
            Ok(vec![])
        }
    }

    fn make_test_client(provider: Arc<dyn LlmProvider>) -> AiClient {
        let privacy = PrivacyFilter::new(&nova_config::AiPrivacyConfig::default())
            .expect("default privacy config is valid");

        let endpoint =
            url::Url::parse("http://user:pass@localhost/?token=supersecret").expect("valid url");

        AiClient {
            provider_kind: AiProviderKind::Ollama,
            provider,
            semaphore: Arc::new(Semaphore::new(1)),
            privacy,
            default_max_tokens: 128,
            default_temperature: None,
            request_timeout: Duration::from_secs(30),
            audit_enabled: true,
            provider_label: "dummy",
            model: "dummy-model".to_string(),
            endpoint,
            azure_cache_key: None,
            cache: None,
            in_flight: Arc::new(TokioMutex::new(HashMap::new())),
            retry: RetryConfig::default(),
        }
    }

    fn audit_events(events: &[CapturedEvent]) -> Vec<CapturedEvent> {
        events
            .iter()
            .filter(|event| event.target == nova_config::AI_AUDIT_TARGET)
            .cloned()
            .collect()
    }

    #[test]
    fn cache_key_distinguishes_temperature_none_from_zero() {
        let client = make_test_client(Arc::new(DummyProvider));
        let messages = vec![ChatMessage::user("hello".to_string())];

        let request_without_temp = ChatRequest {
            messages: messages.clone(),
            max_tokens: Some(16),
            temperature: None,
        };
        let request_with_zero_temp = ChatRequest {
            messages,
            max_tokens: Some(16),
            temperature: Some(0.0),
        };

        let key_without = client.build_cache_key(&request_without_temp);
        let key_zero = client.build_cache_key(&request_with_zero_temp);

        assert_ne!(
            key_without, key_zero,
            "expected cache keys to differ for temperature=None vs temperature=Some(0.0)"
        );
        assert_eq!(
            key_zero,
            client.build_cache_key(&request_with_zero_temp),
            "expected cache key to be stable for temperature=Some(0.0)"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn chat_emits_audit_events_with_sanitized_content() {
        let events = Arc::new(Mutex::new(Vec::<CapturedEvent>::new()));
        let layer = CapturingLayer {
            events: events.clone(),
        };

        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        let client = make_test_client(Arc::new(DummyProvider));
        let secret = SECRET;

        let completion = client
            .chat(
                ChatRequest {
                    messages: vec![crate::types::ChatMessage::user(format!("hello {secret}"))],
                    max_tokens: None,
                    temperature: None,
                },
                CancellationToken::new(),
            )
            .await
            .expect("chat succeeds");

        assert!(
            completion.contains(secret),
            "dummy returns unsanitized content"
        );

        let events = events.lock().unwrap();
        let audit = audit_events(&events);

        let request = audit
            .iter()
            .find(|event| event.fields.get("event").map(String::as_str) == Some("llm_request"))
            .expect("request audit event emitted");
        let request_id = request
            .fields
            .get("request_id")
            .expect("request_id field present")
            .to_string();
        let endpoint = request
            .fields
            .get("endpoint")
            .expect("endpoint field present");
        assert!(!endpoint.contains("token="));
        assert!(!endpoint.contains("supersecret"));
        assert!(!endpoint.contains("user:pass@"));
        let prompt = request.fields.get("prompt").expect("prompt field present");
        assert!(
            prompt.contains("[REDACTED]"),
            "expected prompt to be redacted in audit logs"
        );
        assert!(
            !prompt.contains(secret),
            "expected secret to be removed from audit prompt"
        );

        let response = audit
            .iter()
            .find(|event| event.fields.get("event").map(String::as_str) == Some("llm_response"))
            .expect("response audit event emitted");
        assert_eq!(
            response.fields.get("request_id").map(String::as_str),
            Some(request_id.as_str()),
            "request_id should correlate request/response"
        );
        let completion = response
            .fields
            .get("completion")
            .expect("completion field present");
        assert!(
            completion.contains("[REDACTED]"),
            "expected completion to be redacted in audit logs"
        );
        assert!(
            !completion.contains(secret),
            "expected secret to be removed from audit completion"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn chat_stream_emits_final_audit_event_with_concatenated_completion() {
        let events = Arc::new(Mutex::new(Vec::<CapturedEvent>::new()));
        let layer = CapturingLayer {
            events: events.clone(),
        };

        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        let client = make_test_client(Arc::new(DummyProvider));
        let secret = SECRET;

        let stream = client
            .chat_stream(
                ChatRequest {
                    messages: vec![crate::types::ChatMessage::user(format!("hello {secret}"))],
                    max_tokens: None,
                    temperature: None,
                },
                CancellationToken::new(),
            )
            .await
            .expect("stream starts");

        let parts: Vec<String> = stream.try_collect().await.expect("stream ok");
        assert_eq!(parts.concat(), format!("chunk {secret}"));

        let events = events.lock().unwrap();
        let audit = audit_events(&events);

        let response = audit
            .iter()
            .find(|event| event.fields.get("event").map(String::as_str) == Some("llm_response"))
            .expect("response audit event emitted");
        let request_id = audit
            .iter()
            .find(|event| event.fields.get("event").map(String::as_str) == Some("llm_request"))
            .and_then(|event| event.fields.get("request_id"))
            .expect("request_id field present");
        let endpoint = audit
            .iter()
            .find(|event| event.fields.get("event").map(String::as_str) == Some("llm_request"))
            .and_then(|event| event.fields.get("endpoint"))
            .expect("endpoint field present");
        assert!(!endpoint.contains("token="));
        assert!(!endpoint.contains("supersecret"));
        assert!(!endpoint.contains("user:pass@"));
        assert_eq!(
            response.fields.get("request_id").map(String::as_str),
            Some(request_id.as_str()),
            "request_id should correlate request/response"
        );
        assert_eq!(
            response.fields.get("stream").map(String::as_str),
            Some("true"),
            "expected stream=true on response"
        );
        let completion = response
            .fields
            .get("completion")
            .expect("completion field present");
        assert!(completion.contains("[REDACTED]"));
        assert!(!completion.contains(secret));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn chat_stream_cancel_interrupts_idle_provider_stream() {
        let client = make_test_client(Arc::new(NeverYieldingProvider));
        let cancel = CancellationToken::new();
        let cancel_for_test = cancel.clone();

        let mut stream = client
            .chat_stream(
                ChatRequest {
                    messages: vec![crate::types::ChatMessage::user("hello".to_string())],
                    max_tokens: None,
                    temperature: None,
                },
                cancel,
            )
            .await
            .expect("stream starts");

        cancel_for_test.cancel();

        let err = tokio::time::timeout(Duration::from_millis(250), stream.try_next())
            .await
            .expect("stream should observe cancellation promptly")
            .expect_err("expected cancellation error");

        assert!(matches!(err, AiError::Cancelled), "{err:?}");
    }

    #[derive(Default)]
    struct CapturedRequest {
        request: Mutex<Option<ChatRequest>>,
    }

    struct CapturingProvider {
        captured: Arc<CapturedRequest>,
    }

    #[async_trait]
    impl LlmProvider for CapturingProvider {
        async fn chat(
            &self,
            request: ChatRequest,
            _cancel: CancellationToken,
        ) -> Result<String, AiError> {
            *self.captured.request.lock().unwrap() = Some(request);
            Ok("ok".to_string())
        }

        async fn chat_stream(
            &self,
            request: ChatRequest,
            _cancel: CancellationToken,
        ) -> Result<AiStream, AiError> {
            *self.captured.request.lock().unwrap() = Some(request);
            let stream = async_stream::try_stream! {
                yield "ok".to_string();
            };
            Ok(Box::pin(stream))
        }

        async fn list_models(&self, _cancel: CancellationToken) -> Result<Vec<String>, AiError> {
            Ok(vec![])
        }
    }

    #[tokio::test]
    async fn chat_sanitization_is_stable_across_messages_and_snippets() {
        let captured = Arc::new(CapturedRequest::default());
        let provider: Arc<dyn LlmProvider> = Arc::new(CapturingProvider {
            captured: captured.clone(),
        });

        let privacy_cfg = AiPrivacyConfig {
            local_only: false,
            anonymize_identifiers: Some(true),
            ..AiPrivacyConfig::default()
        };
        let privacy = PrivacyFilter::new(&privacy_cfg).expect("privacy filter");

        let client = AiClient {
            provider_kind: AiProviderKind::Ollama,
            provider,
            semaphore: Arc::new(Semaphore::new(1)),
            privacy,
            default_max_tokens: 16,
            default_temperature: None,
            request_timeout: Duration::from_secs(30),
            audit_enabled: false,
            provider_label: "dummy",
            model: "dummy-model".to_string(),
            endpoint: url::Url::parse("http://localhost").expect("valid url"),
            azure_cache_key: None,
            cache: None,
            in_flight: Arc::new(TokioMutex::new(HashMap::new())),
            retry: RetryConfig::default(),
        };

        let request = ChatRequest {
            messages: vec![
                crate::types::ChatMessage::user(
                    "Snippet 1:\n```java\nimport java.util.List;\nclass Foo {\n  // secret token\n  java.util.List<String> list = null;\n}\n```\n",
                ),
                crate::types::ChatMessage::user("Snippet 2:\n```java\nFoo foo = null;\n```\n"),
            ],
            max_tokens: None,
            temperature: None,
        };

        let _ = client
            .chat(request, CancellationToken::new())
            .await
            .expect("chat");

        let req = captured
            .request
            .lock()
            .expect("captured request mutex poisoned")
            .take()
            .expect("provider should receive request");
        let msg1 = &req.messages[0].content;
        let msg2 = &req.messages[1].content;

        // Stdlib fully-qualified names should remain readable.
        assert!(msg1.contains("java.util.List"), "{msg1}");

        // Identifiers should be anonymized consistently across snippets.
        assert!(!msg1.contains("Foo"), "{msg1}");
        assert!(!msg2.contains("Foo"), "{msg2}");
        assert!(msg1.contains("class id_0"), "{msg1}");
        assert!(msg2.contains("id_0"), "{msg2}");

        // Comments should be stripped when anonymization is enabled.
        assert!(msg1.contains("// [REDACTED]"), "{msg1}");
        assert!(!msg1.contains("secret token"), "{msg1}");
    }

    #[derive(Clone)]
    struct CountingProvider {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl LlmProvider for CountingProvider {
        async fn chat(
            &self,
            _request: ChatRequest,
            _cancel: CancellationToken,
        ) -> Result<String, AiError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok("Pong".to_string())
        }

        async fn chat_stream(
            &self,
            _request: ChatRequest,
            _cancel: CancellationToken,
        ) -> Result<AiStream, AiError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let stream = async_stream::try_stream! {
                yield "Pong".to_string();
            };
            Ok(Box::pin(stream))
        }

        async fn list_models(&self, _cancel: CancellationToken) -> Result<Vec<String>, AiError> {
            Ok(vec![])
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn audit_logging_does_not_change_cache_key() {
        // Regression test for an earlier bug where enabling audit logging would apply
        // `audit::sanitize_prompt_for_audit` *before* sending to the provider, which also changed
        // the cache key (since cache keys are computed from the provider-visible prompt).
        //
        // Here we use a cache shared between two clients and ensure the second request hits the
        // cache even when audit logging is enabled.
        let calls = Arc::new(AtomicUsize::new(0));
        let provider: Arc<dyn LlmProvider> = Arc::new(CountingProvider {
            calls: calls.clone(),
        });

        let cache = Arc::new(LlmResponseCache::new(CacheSettings {
            max_entries: 16,
            ttl: Duration::from_secs(60),
        }));

        let endpoint = url::Url::parse("http://localhost").expect("valid url");
        let request = ChatRequest {
            messages: vec![crate::types::ChatMessage::user(format!("hello {SECRET}"))],
            max_tokens: None,
            temperature: None,
        };

        let privacy = PrivacyFilter::new(&AiPrivacyConfig::default()).expect("privacy filter");
        let client_no_audit = AiClient {
            provider_kind: AiProviderKind::Ollama,
            provider: provider.clone(),
            semaphore: Arc::new(Semaphore::new(1)),
            privacy,
            default_max_tokens: 128,
            default_temperature: None,
            request_timeout: Duration::from_secs(30),
            audit_enabled: false,
            provider_label: "dummy",
            model: "dummy-model".to_string(),
            endpoint: endpoint.clone(),
            azure_cache_key: None,
            cache: Some(cache.clone()),
            in_flight: Arc::new(TokioMutex::new(HashMap::new())),
            retry: RetryConfig::default(),
        };

        let privacy = PrivacyFilter::new(&AiPrivacyConfig::default()).expect("privacy filter");
        let client_with_audit = AiClient {
            provider_kind: AiProviderKind::Ollama,
            provider,
            semaphore: Arc::new(Semaphore::new(1)),
            privacy,
            default_max_tokens: 128,
            default_temperature: None,
            request_timeout: Duration::from_secs(30),
            audit_enabled: true,
            provider_label: "dummy",
            model: "dummy-model".to_string(),
            endpoint,
            azure_cache_key: None,
            cache: Some(cache.clone()),
            in_flight: Arc::new(TokioMutex::new(HashMap::new())),
            retry: RetryConfig::default(),
        };

        let out1 = client_no_audit
            .chat(request.clone(), CancellationToken::new())
            .await
            .expect("chat succeeds");
        let out2 = client_with_audit
            .chat(request, CancellationToken::new())
            .await
            .expect("chat succeeds");

        assert_eq!(out1, "Pong");
        assert_eq!(out2, "Pong");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "expected second request to hit shared cache despite audit logging"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cache_hit_increments_metrics() {
        let _guard = crate::test_support::metrics_lock()
            .lock()
            .expect("metrics lock poisoned");
        let metrics = nova_metrics::MetricsRegistry::global();
        let before = metrics
            .snapshot()
            .methods
            .get(AI_CHAT_CACHE_HIT_METRIC)
            .map(|m| m.request_count)
            .unwrap_or(0);

        let calls = Arc::new(AtomicUsize::new(0));
        let provider: Arc<dyn LlmProvider> = Arc::new(CountingProvider {
            calls: calls.clone(),
        });
        let cache = Arc::new(LlmResponseCache::new(CacheSettings {
            max_entries: 16,
            ttl: Duration::from_secs(60),
        }));

        let privacy = PrivacyFilter::new(&AiPrivacyConfig::default()).expect("privacy filter");
        let client = AiClient {
            provider_kind: AiProviderKind::Ollama,
            provider,
            semaphore: Arc::new(Semaphore::new(1)),
            privacy,
            default_max_tokens: 128,
            default_temperature: None,
            request_timeout: Duration::from_secs(30),
            audit_enabled: false,
            provider_label: "dummy",
            model: "dummy-model".to_string(),
            endpoint: url::Url::parse("http://localhost").expect("valid url"),
            azure_cache_key: None,
            cache: Some(cache),
            in_flight: Arc::new(TokioMutex::new(HashMap::new())),
            retry: RetryConfig::default(),
        };

        let request = ChatRequest {
            messages: vec![crate::types::ChatMessage::user("hello".to_string())],
            max_tokens: None,
            temperature: None,
        };

        let out1 = client
            .chat(request.clone(), CancellationToken::new())
            .await
            .expect("chat succeeds");
        let out2 = client
            .chat(request, CancellationToken::new())
            .await
            .expect("chat succeeds");

        assert_eq!(out1, "Pong");
        assert_eq!(out2, "Pong");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "expected second request to hit cache"
        );

        let after = metrics
            .snapshot()
            .methods
            .get(AI_CHAT_CACHE_HIT_METRIC)
            .map(|m| m.request_count)
            .unwrap_or(0);
        assert!(
            after >= before.saturating_add(1),
            "expected {AI_CHAT_CACHE_HIT_METRIC} to increment"
        );
    }

    #[derive(Clone, Default)]
    struct SlowProvider;

    #[async_trait]
    impl LlmProvider for SlowProvider {
        async fn chat(
            &self,
            _request: ChatRequest,
            _cancel: CancellationToken,
        ) -> Result<String, AiError> {
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok("Too slow".to_string())
        }

        async fn chat_stream(
            &self,
            _request: ChatRequest,
            _cancel: CancellationToken,
        ) -> Result<AiStream, AiError> {
            Err(AiError::UnexpectedResponse(
                "SlowProvider does not support streaming".to_string(),
            ))
        }

        async fn list_models(&self, _cancel: CancellationToken) -> Result<Vec<String>, AiError> {
            Ok(vec![])
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn timeout_increments_metrics() {
        let _guard = crate::test_support::metrics_lock()
            .lock()
            .expect("metrics lock poisoned");
        let metrics = nova_metrics::MetricsRegistry::global();
        let before = metrics
            .snapshot()
            .methods
            .get(AI_CHAT_METRIC)
            .map(|m| m.timeout_count)
            .unwrap_or(0);

        let privacy = PrivacyFilter::new(&AiPrivacyConfig::default()).expect("privacy filter");
        let client = AiClient {
            provider_kind: AiProviderKind::Ollama,
            provider: Arc::new(SlowProvider),
            semaphore: Arc::new(Semaphore::new(1)),
            privacy,
            default_max_tokens: 128,
            default_temperature: None,
            request_timeout: Duration::from_millis(10),
            audit_enabled: false,
            provider_label: "dummy",
            model: "dummy-model".to_string(),
            endpoint: url::Url::parse("http://localhost").expect("valid url"),
            azure_cache_key: None,
            cache: None,
            in_flight: Arc::new(TokioMutex::new(HashMap::new())),
            retry: RetryConfig::default(),
        };

        let request = ChatRequest {
            messages: vec![crate::types::ChatMessage::user("hello".to_string())],
            max_tokens: None,
            temperature: None,
        };

        let err = client
            .chat(request, CancellationToken::new())
            .await
            .expect_err("expected timeout");
        assert!(matches!(err, AiError::Timeout));

        let after = metrics
            .snapshot()
            .methods
            .get(AI_CHAT_METRIC)
            .map(|m| m.timeout_count)
            .unwrap_or(0);
        assert!(
            after >= before.saturating_add(1),
            "expected {AI_CHAT_METRIC} timeout_count to increment"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reqwest_timeout_wrapped_as_http_is_still_classified_as_timeout_in_metrics() {
        use hyper::service::{make_service_fn, service_fn};
        use hyper::{Body, Response, Server};
        use std::convert::Infallible;
        use std::net::TcpListener;
        use tokio::sync::oneshot;

        let _guard = crate::test_support::metrics_lock()
            .lock()
            .expect("metrics lock poisoned");

        let metrics = MetricsRegistry::global();
        metrics.reset();

        // Create a real `reqwest::Error` with `is_timeout() == true`.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("listener addr");
        listener
            .set_nonblocking(true)
            .expect("set listener nonblocking");

        let make_svc = make_service_fn(|_conn| async {
            Ok::<_, Infallible>(service_fn(|_req| async {
                tokio::time::sleep(Duration::from_millis(200)).await;
                Ok::<_, Infallible>(Response::new(Body::from("ok")))
            }))
        });

        let server = Server::from_tcp(listener)
            .expect("server from_tcp")
            .serve(make_svc);
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let server_handle = tokio::spawn(server.with_graceful_shutdown(async {
            let _ = shutdown_rx.await;
        }));

        let client = reqwest::Client::new();
        let url = format!("http://{addr}/");
        let timeout_err = client
            .get(url)
            .timeout(Duration::from_millis(50))
            .send()
            .await
            .expect_err("expected timeout");
        assert!(timeout_err.is_timeout(), "expected reqwest timeout error");

        let err = AiError::Http(Arc::new(timeout_err));
        record_chat_error_metrics(metrics, &err);

        let snap = metrics.snapshot();
        let chat = snap
            .methods
            .get(AI_CHAT_METRIC)
            .expect("expected ai/chat metric");
        assert_eq!(chat.timeout_count, 1);
        assert_eq!(chat.error_count, 0);

        let timeout_metric = snap
            .methods
            .get(AI_CHAT_ERROR_TIMEOUT_METRIC)
            .expect("expected ai/chat/error/timeout metric");
        assert_eq!(timeout_metric.timeout_count, 1);

        let http_errors = snap
            .methods
            .get(AI_CHAT_ERROR_HTTP_METRIC)
            .map(|m| m.error_count)
            .unwrap_or(0);
        assert_eq!(http_errors, 0);

        metrics.reset();

        let _ = shutdown_tx.send(());
        let _ = server_handle.await;
    }

    #[derive(Clone, Default)]
    struct OkStreamProvider;

    #[async_trait]
    impl LlmProvider for OkStreamProvider {
        async fn chat(
            &self,
            _request: ChatRequest,
            _cancel: CancellationToken,
        ) -> Result<String, AiError> {
            Ok("ok".to_string())
        }

        async fn chat_stream(
            &self,
            _request: ChatRequest,
            _cancel: CancellationToken,
        ) -> Result<AiStream, AiError> {
            let stream = async_stream::try_stream! {
                yield "ok".to_string();
            };
            Ok(Box::pin(stream))
        }

        async fn list_models(&self, _cancel: CancellationToken) -> Result<Vec<String>, AiError> {
            Ok(Vec::new())
        }
    }

    #[derive(Clone, Default)]
    struct TimeoutStreamProvider;

    #[async_trait]
    impl LlmProvider for TimeoutStreamProvider {
        async fn chat(
            &self,
            _request: ChatRequest,
            _cancel: CancellationToken,
        ) -> Result<String, AiError> {
            Ok("ok".to_string())
        }

        async fn chat_stream(
            &self,
            _request: ChatRequest,
            _cancel: CancellationToken,
        ) -> Result<AiStream, AiError> {
            let stream =
                futures::stream::once(async { Err::<String, AiError>(AiError::Timeout) });
            Ok(Box::pin(stream))
        }

        async fn list_models(&self, _cancel: CancellationToken) -> Result<Vec<String>, AiError> {
            Ok(Vec::new())
        }
    }

    #[derive(Clone, Default)]
    struct CancelledStreamProvider;

    #[async_trait]
    impl LlmProvider for CancelledStreamProvider {
        async fn chat(
            &self,
            _request: ChatRequest,
            _cancel: CancellationToken,
        ) -> Result<String, AiError> {
            Ok("ok".to_string())
        }

        async fn chat_stream(
            &self,
            _request: ChatRequest,
            _cancel: CancellationToken,
        ) -> Result<AiStream, AiError> {
            let stream =
                futures::stream::once(async { Err::<String, AiError>(AiError::Cancelled) });
            Ok(Box::pin(stream))
        }

        async fn list_models(&self, _cancel: CancellationToken) -> Result<Vec<String>, AiError> {
            Ok(Vec::new())
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn chat_stream_success_increments_metrics() {
        let _guard = crate::test_support::metrics_lock()
            .lock()
            .expect("metrics lock poisoned");
        let metrics = nova_metrics::MetricsRegistry::global();
        metrics.reset();
        let before = metrics
            .snapshot()
            .methods
            .get(AI_CHAT_STREAM_METRIC)
            .map(|m| m.request_count)
            .unwrap_or(0);

        let privacy = PrivacyFilter::new(&AiPrivacyConfig::default()).expect("privacy filter");
        let client = AiClient {
            provider_kind: AiProviderKind::Ollama,
            provider: Arc::new(OkStreamProvider),
            semaphore: Arc::new(Semaphore::new(1)),
            privacy,
            default_max_tokens: 128,
            default_temperature: None,
            request_timeout: Duration::from_secs(30),
            audit_enabled: false,
            provider_label: "dummy",
            model: "dummy-model".to_string(),
            endpoint: url::Url::parse("http://localhost").expect("valid url"),
            azure_cache_key: None,
            cache: None,
            in_flight: Arc::new(TokioMutex::new(HashMap::new())),
            retry: RetryConfig::default(),
        };

        let request = ChatRequest {
            messages: vec![crate::types::ChatMessage::user("hello".to_string())],
            max_tokens: None,
            temperature: None,
        };

        let stream = client
            .chat_stream(request, CancellationToken::new())
            .await
            .expect("stream starts");
        let parts: Vec<String> = stream.try_collect().await.expect("stream ok");
        assert_eq!(parts.concat(), "ok");

        let after = metrics
            .snapshot()
            .methods
            .get(AI_CHAT_STREAM_METRIC)
            .map(|m| m.request_count)
            .unwrap_or(0);
        assert!(
            after >= before.saturating_add(1),
            "expected {AI_CHAT_STREAM_METRIC} request_count to increment"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn chat_stream_timeout_increments_metrics() {
        let _guard = crate::test_support::metrics_lock()
            .lock()
            .expect("metrics lock poisoned");
        let metrics = nova_metrics::MetricsRegistry::global();
        metrics.reset();
        let before = metrics
            .snapshot()
            .methods
            .get(AI_CHAT_STREAM_METRIC)
            .map(|m| m.timeout_count)
            .unwrap_or(0);

        let privacy = PrivacyFilter::new(&AiPrivacyConfig::default()).expect("privacy filter");
        let client = AiClient {
            provider_kind: AiProviderKind::Ollama,
            provider: Arc::new(TimeoutStreamProvider),
            semaphore: Arc::new(Semaphore::new(1)),
            privacy,
            default_max_tokens: 128,
            default_temperature: None,
            request_timeout: Duration::from_secs(30),
            audit_enabled: false,
            provider_label: "dummy",
            model: "dummy-model".to_string(),
            endpoint: url::Url::parse("http://localhost").expect("valid url"),
            azure_cache_key: None,
            cache: None,
            in_flight: Arc::new(TokioMutex::new(HashMap::new())),
            retry: RetryConfig::default(),
        };

        let request = ChatRequest {
            messages: vec![crate::types::ChatMessage::user("hello".to_string())],
            max_tokens: None,
            temperature: None,
        };

        let stream = client
            .chat_stream(request, CancellationToken::new())
            .await
            .expect("stream starts");
        let err = stream
            .try_collect::<Vec<String>>()
            .await
            .expect_err("expected timeout");
        assert!(matches!(err, AiError::Timeout));

        let after = metrics
            .snapshot()
            .methods
            .get(AI_CHAT_STREAM_METRIC)
            .map(|m| m.timeout_count)
            .unwrap_or(0);
        assert!(
            after >= before.saturating_add(1),
            "expected {AI_CHAT_STREAM_METRIC} timeout_count to increment"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn chat_stream_cancelled_increments_metrics() {
        let _guard = crate::test_support::metrics_lock()
            .lock()
            .expect("metrics lock poisoned");
        let metrics = nova_metrics::MetricsRegistry::global();
        metrics.reset();
        let before = metrics
            .snapshot()
            .methods
            .get(AI_CHAT_STREAM_METRIC)
            .map(|m| m.error_count)
            .unwrap_or(0);

        let privacy = PrivacyFilter::new(&AiPrivacyConfig::default()).expect("privacy filter");
        let client = AiClient {
            provider_kind: AiProviderKind::Ollama,
            provider: Arc::new(CancelledStreamProvider),
            semaphore: Arc::new(Semaphore::new(1)),
            privacy,
            default_max_tokens: 128,
            default_temperature: None,
            request_timeout: Duration::from_secs(30),
            audit_enabled: false,
            provider_label: "dummy",
            model: "dummy-model".to_string(),
            endpoint: url::Url::parse("http://localhost").expect("valid url"),
            azure_cache_key: None,
            cache: None,
            in_flight: Arc::new(TokioMutex::new(HashMap::new())),
            retry: RetryConfig::default(),
        };

        let request = ChatRequest {
            messages: vec![crate::types::ChatMessage::user("hello".to_string())],
            max_tokens: None,
            temperature: None,
        };

        let stream = client
            .chat_stream(request, CancellationToken::new())
            .await
            .expect("stream starts");
        let err = stream
            .try_collect::<Vec<String>>()
            .await
            .expect_err("expected cancellation");
        assert!(matches!(err, AiError::Cancelled));

        let after = metrics
            .snapshot()
            .methods
            .get(AI_CHAT_STREAM_METRIC)
            .map(|m| m.error_count)
            .unwrap_or(0);
        assert!(
            after >= before.saturating_add(1),
            "expected {AI_CHAT_STREAM_METRIC} error_count to increment"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reqwest_timeout_wrapped_as_http_is_classified_as_timeout_in_stream_metrics() {
        use hyper::service::{make_service_fn, service_fn};
        use hyper::{Body, Response, Server};
        use std::convert::Infallible;
        use std::net::TcpListener;
        use tokio::sync::oneshot;

        // Use a standalone metrics registry to avoid cross-test interference.
        let metrics = MetricsRegistry::default();

        // Create a real `reqwest::Error` with `is_timeout() == true`.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("listener addr");
        listener
            .set_nonblocking(true)
            .expect("set listener nonblocking");

        let make_svc = make_service_fn(|_conn| async {
            Ok::<_, Infallible>(service_fn(|_req| async {
                tokio::time::sleep(Duration::from_millis(200)).await;
                Ok::<_, Infallible>(Response::new(Body::from("ok")))
            }))
        });

        let server = Server::from_tcp(listener)
            .expect("server from_tcp")
            .serve(make_svc);
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let server_handle = tokio::spawn(server.with_graceful_shutdown(async {
            let _ = shutdown_rx.await;
        }));

        let client = reqwest::Client::new();
        let url = format!("http://{addr}/");
        let timeout_err = client
            .get(url)
            .timeout(Duration::from_millis(50))
            .send()
            .await
            .expect_err("expected timeout");
        assert!(timeout_err.is_timeout(), "expected reqwest timeout error");

        let err = AiError::Http(Arc::new(timeout_err));
        record_chat_stream_error_metrics(&metrics, &err);

        let snap = metrics.snapshot();
        let stream = snap
            .methods
            .get(AI_CHAT_STREAM_METRIC)
            .expect("expected ai/chat_stream metric");
        assert_eq!(stream.timeout_count, 1);
        assert_eq!(stream.error_count, 0);

        let timeout_metric = snap
            .methods
            .get(AI_CHAT_STREAM_ERROR_TIMEOUT_METRIC)
            .expect("expected ai/chat_stream/error/timeout metric");
        assert_eq!(timeout_metric.timeout_count, 1);

        let http_errors = snap
            .methods
            .get(AI_CHAT_STREAM_ERROR_HTTP_METRIC)
            .map(|m| m.error_count)
            .unwrap_or(0);
        assert_eq!(http_errors, 0);

        let _ = shutdown_tx.send(());
        let _ = server_handle.await;
    }

    #[derive(Clone, Default)]
    struct UnexpectedResponseStreamProvider;

    #[async_trait]
    impl LlmProvider for UnexpectedResponseStreamProvider {
        async fn chat(
            &self,
            _request: ChatRequest,
            _cancel: CancellationToken,
        ) -> Result<String, AiError> {
            Ok("ok".to_string())
        }

        async fn chat_stream(
            &self,
            _request: ChatRequest,
            _cancel: CancellationToken,
        ) -> Result<AiStream, AiError> {
            let stream = async_stream::try_stream! {
                yield "chunk".to_string();
                Err(AiError::UnexpectedResponse("boom".to_string()))?;
            };
            Ok(Box::pin(stream))
        }

        async fn list_models(&self, _cancel: CancellationToken) -> Result<Vec<String>, AiError> {
            Ok(Vec::new())
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn chat_stream_mid_stream_error_increments_error_metric() {
        let _guard = crate::test_support::metrics_lock()
            .lock()
            .expect("metrics lock poisoned");
        let metrics = nova_metrics::MetricsRegistry::global();
        metrics.reset();

        let before_requests = metrics
            .snapshot()
            .methods
            .get(AI_CHAT_STREAM_METRIC)
            .map(|m| m.request_count)
            .unwrap_or(0);
        let before_error_metric = metrics
            .snapshot()
            .methods
            .get(AI_CHAT_STREAM_ERROR_UNEXPECTED_RESPONSE_METRIC)
            .map(|m| m.error_count)
            .unwrap_or(0);

        let privacy = PrivacyFilter::new(&AiPrivacyConfig::default()).expect("privacy filter");
        let client = AiClient {
            provider_kind: AiProviderKind::Ollama,
            provider: Arc::new(UnexpectedResponseStreamProvider),
            semaphore: Arc::new(Semaphore::new(1)),
            privacy,
            default_max_tokens: 128,
            default_temperature: None,
            request_timeout: Duration::from_secs(30),
            audit_enabled: false,
            provider_label: "dummy",
            model: "dummy-model".to_string(),
            endpoint: url::Url::parse("http://localhost").expect("valid url"),
            azure_cache_key: None,
            cache: None,
            in_flight: Arc::new(TokioMutex::new(HashMap::new())),
            retry: RetryConfig::default(),
        };

        let request = ChatRequest {
            messages: vec![crate::types::ChatMessage::user("hello".to_string())],
            max_tokens: None,
            temperature: None,
        };

        let stream = client
            .chat_stream(request, CancellationToken::new())
            .await
            .expect("stream starts");
        let err = stream
            .try_collect::<Vec<String>>()
            .await
            .expect_err("expected error");
        assert!(matches!(err, AiError::UnexpectedResponse(_)));

        let after_requests = metrics
            .snapshot()
            .methods
            .get(AI_CHAT_STREAM_METRIC)
            .map(|m| m.request_count)
            .unwrap_or(0);
        let after_error_metric = metrics
            .snapshot()
            .methods
            .get(AI_CHAT_STREAM_ERROR_UNEXPECTED_RESPONSE_METRIC)
            .map(|m| m.error_count)
            .unwrap_or(0);

        assert!(
            after_requests >= before_requests.saturating_add(1),
            "expected {AI_CHAT_STREAM_METRIC} request_count to increment"
        );
        assert!(
            after_error_metric >= before_error_metric.saturating_add(1),
            "expected {AI_CHAT_STREAM_ERROR_UNEXPECTED_RESPONSE_METRIC} error_count to increment"
        );
    }

    #[derive(Clone, Default)]
    struct ListModelsOkProvider;

    #[async_trait]
    impl LlmProvider for ListModelsOkProvider {
        async fn chat(
            &self,
            _request: ChatRequest,
            _cancel: CancellationToken,
        ) -> Result<String, AiError> {
            Ok("ok".to_string())
        }

        async fn chat_stream(
            &self,
            _request: ChatRequest,
            _cancel: CancellationToken,
        ) -> Result<AiStream, AiError> {
            let stream = async_stream::try_stream! {
                yield "ok".to_string();
            };
            Ok(Box::pin(stream))
        }

        async fn list_models(&self, _cancel: CancellationToken) -> Result<Vec<String>, AiError> {
            Ok(vec!["dummy".to_string()])
        }
    }

    #[derive(Clone, Default)]
    struct ListModelsTimeoutProvider;

    #[async_trait]
    impl LlmProvider for ListModelsTimeoutProvider {
        async fn chat(
            &self,
            _request: ChatRequest,
            _cancel: CancellationToken,
        ) -> Result<String, AiError> {
            Ok("ok".to_string())
        }

        async fn chat_stream(
            &self,
            _request: ChatRequest,
            _cancel: CancellationToken,
        ) -> Result<AiStream, AiError> {
            let stream = async_stream::try_stream! {
                yield "ok".to_string();
            };
            Ok(Box::pin(stream))
        }

        async fn list_models(&self, _cancel: CancellationToken) -> Result<Vec<String>, AiError> {
            Err(AiError::Timeout)
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn list_models_success_increments_metrics() {
        let _guard = crate::test_support::metrics_lock()
            .lock()
            .expect("metrics lock poisoned");
        let metrics = nova_metrics::MetricsRegistry::global();
        metrics.reset();

        let before = metrics
            .snapshot()
            .methods
            .get(AI_LIST_MODELS_METRIC)
            .map(|m| m.request_count)
            .unwrap_or(0);

        let privacy = PrivacyFilter::new(&AiPrivacyConfig::default()).expect("privacy filter");
        let client = AiClient {
            provider_kind: AiProviderKind::Ollama,
            provider: Arc::new(ListModelsOkProvider),
            semaphore: Arc::new(Semaphore::new(1)),
            privacy,
            default_max_tokens: 128,
            default_temperature: None,
            request_timeout: Duration::from_secs(30),
            audit_enabled: false,
            provider_label: "dummy",
            model: "dummy-model".to_string(),
            endpoint: url::Url::parse("http://localhost").expect("valid url"),
            azure_cache_key: None,
            cache: None,
            in_flight: Arc::new(TokioMutex::new(HashMap::new())),
            retry: RetryConfig::default(),
        };

        let models = client
            .list_models(CancellationToken::new())
            .await
            .expect("list_models succeeds");
        assert_eq!(models, vec!["dummy".to_string()]);

        let after = metrics
            .snapshot()
            .methods
            .get(AI_LIST_MODELS_METRIC)
            .map(|m| m.request_count)
            .unwrap_or(0);
        assert!(
            after >= before.saturating_add(1),
            "expected {AI_LIST_MODELS_METRIC} request_count to increment"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn list_models_timeout_increments_metrics() {
        let _guard = crate::test_support::metrics_lock()
            .lock()
            .expect("metrics lock poisoned");
        let metrics = nova_metrics::MetricsRegistry::global();
        metrics.reset();

        let before_requests = metrics
            .snapshot()
            .methods
            .get(AI_LIST_MODELS_METRIC)
            .map(|m| m.request_count)
            .unwrap_or(0);
        let before_timeouts = metrics
            .snapshot()
            .methods
            .get(AI_LIST_MODELS_METRIC)
            .map(|m| m.timeout_count)
            .unwrap_or(0);
        let before_timeout_metric = metrics
            .snapshot()
            .methods
            .get(AI_LIST_MODELS_ERROR_TIMEOUT_METRIC)
            .map(|m| m.timeout_count)
            .unwrap_or(0);

        let privacy = PrivacyFilter::new(&AiPrivacyConfig::default()).expect("privacy filter");
        let client = AiClient {
            provider_kind: AiProviderKind::Ollama,
            provider: Arc::new(ListModelsTimeoutProvider),
            semaphore: Arc::new(Semaphore::new(1)),
            privacy,
            default_max_tokens: 128,
            default_temperature: None,
            request_timeout: Duration::from_secs(30),
            audit_enabled: false,
            provider_label: "dummy",
            model: "dummy-model".to_string(),
            endpoint: url::Url::parse("http://localhost").expect("valid url"),
            azure_cache_key: None,
            cache: None,
            in_flight: Arc::new(TokioMutex::new(HashMap::new())),
            retry: RetryConfig {
                max_retries: 0,
                initial_backoff: Duration::ZERO,
                max_backoff: Duration::ZERO,
            },
        };

        let err = client
            .list_models(CancellationToken::new())
            .await
            .expect_err("expected timeout");
        assert!(matches!(err, AiError::Timeout));

        let after_requests = metrics
            .snapshot()
            .methods
            .get(AI_LIST_MODELS_METRIC)
            .map(|m| m.request_count)
            .unwrap_or(0);
        let after_timeouts = metrics
            .snapshot()
            .methods
            .get(AI_LIST_MODELS_METRIC)
            .map(|m| m.timeout_count)
            .unwrap_or(0);
        let after_timeout_metric = metrics
            .snapshot()
            .methods
            .get(AI_LIST_MODELS_ERROR_TIMEOUT_METRIC)
            .map(|m| m.timeout_count)
            .unwrap_or(0);

        assert!(
            after_requests >= before_requests.saturating_add(1),
            "expected {AI_LIST_MODELS_METRIC} request_count to increment"
        );
        assert!(
            after_timeouts >= before_timeouts.saturating_add(1),
            "expected {AI_LIST_MODELS_METRIC} timeout_count to increment"
        );
        assert!(
            after_timeout_metric >= before_timeout_metric.saturating_add(1),
            "expected {AI_LIST_MODELS_ERROR_TIMEOUT_METRIC} timeout_count to increment"
        );
    }

    #[derive(Clone)]
    struct ListModelsFailOnceProvider {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl LlmProvider for ListModelsFailOnceProvider {
        async fn chat(
            &self,
            _request: ChatRequest,
            _cancel: CancellationToken,
        ) -> Result<String, AiError> {
            Ok("ok".to_string())
        }

        async fn chat_stream(
            &self,
            _request: ChatRequest,
            _cancel: CancellationToken,
        ) -> Result<AiStream, AiError> {
            let stream = async_stream::try_stream! {
                yield "ok".to_string();
            };
            Ok(Box::pin(stream))
        }

        async fn list_models(&self, _cancel: CancellationToken) -> Result<Vec<String>, AiError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                Err(AiError::Timeout)
            } else {
                Ok(vec!["dummy".to_string()])
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn list_models_retries_increment_retry_metric() {
        let _guard = crate::test_support::metrics_lock()
            .lock()
            .expect("metrics lock poisoned");
        let metrics = nova_metrics::MetricsRegistry::global();
        metrics.reset();

        let before = metrics
            .snapshot()
            .methods
            .get(AI_LIST_MODELS_RETRY_METRIC)
            .map(|m| m.request_count)
            .unwrap_or(0);

        let privacy = PrivacyFilter::new(&AiPrivacyConfig::default()).expect("privacy filter");
        let client = AiClient {
            provider_kind: AiProviderKind::Ollama,
            provider: Arc::new(ListModelsFailOnceProvider {
                calls: Arc::new(AtomicUsize::new(0)),
            }),
            semaphore: Arc::new(Semaphore::new(1)),
            privacy,
            default_max_tokens: 128,
            default_temperature: None,
            request_timeout: Duration::from_secs(30),
            audit_enabled: false,
            provider_label: "dummy",
            model: "dummy-model".to_string(),
            endpoint: url::Url::parse("http://localhost").expect("valid url"),
            azure_cache_key: None,
            cache: None,
            in_flight: Arc::new(TokioMutex::new(HashMap::new())),
            retry: RetryConfig {
                max_retries: 1,
                initial_backoff: Duration::ZERO,
                max_backoff: Duration::ZERO,
            },
        };

        let models = client
            .list_models(CancellationToken::new())
            .await
            .expect("list_models succeeds after retry");
        assert_eq!(models, vec!["dummy".to_string()]);

        let after = metrics
            .snapshot()
            .methods
            .get(AI_LIST_MODELS_RETRY_METRIC)
            .map(|m| m.request_count)
            .unwrap_or(0);
        assert!(
            after >= before.saturating_add(1),
            "expected {AI_LIST_MODELS_RETRY_METRIC} request_count to increment"
        );
    }
} 
