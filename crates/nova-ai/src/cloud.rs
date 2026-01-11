use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;
use std::time::Instant;
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};
use url::Url;

use crate::audit;
use crate::cache::{shared_cache, CacheKey, CacheKeyBuilder, CacheSettings, LlmResponseCache};

#[derive(Debug, Error)]
pub enum CloudLlmError {
    #[error("request cancelled")]
    Cancelled,

    #[error("request failed: {0}")]
    Request(#[from] reqwest::Error),

    #[error("invalid configuration: {0}")]
    InvalidConfig(String),

    #[error("provider returned non-success status {status}: {body}")]
    BadStatus { status: StatusCode, body: String },

    #[error("failed to parse provider response: {0}")]
    InvalidResponse(String),
}

#[derive(Debug, Clone)]
pub struct RetryConfig {
    pub max_retries: usize,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
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

#[derive(Debug, Clone)]
pub enum ProviderKind {
    OpenAi,
    Anthropic,
    Gemini,
    AzureOpenAi {
        deployment: String,
        api_version: String,
    },
    /// A simple JSON-over-HTTP API (useful for proxies and tests).
    ///
    /// Request body:
    /// `{ "model": "...", "prompt": "...", "max_tokens": 123, "temperature": 0.2 }`
    ///
    /// Response body:
    /// `{ "completion": "..." }`
    Http,
}

#[derive(Debug, Clone)]
pub struct CloudLlmConfig {
    pub provider: ProviderKind,
    pub endpoint: Url,
    pub api_key: Option<String>,
    pub model: String,
    pub timeout: Duration,
    pub retry: RetryConfig,
    pub audit_logging: bool,
    pub cache_enabled: bool,
    pub cache_max_entries: usize,
    pub cache_ttl: Duration,
}

impl CloudLlmConfig {
    pub fn http(endpoint: Url) -> Self {
        Self {
            provider: ProviderKind::Http,
            endpoint,
            api_key: None,
            model: "default".to_string(),
            timeout: Duration::from_secs(30),
            retry: RetryConfig::default(),
            audit_logging: false,
            cache_enabled: false,
            cache_max_entries: 256,
            cache_ttl: Duration::from_secs(300),
        }
    }
}

#[derive(Debug, Clone)]
pub struct GenerateRequest {
    pub prompt: String,
    pub max_tokens: u32,
    pub temperature: f32,
}

#[derive(Debug, Clone)]
pub struct ProviderRequestParts {
    pub url: Url,
    pub headers: HeaderMap,
    pub body: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct CloudLlmClient {
    cfg: CloudLlmConfig,
    http: reqwest::Client,
    cache: Option<std::sync::Arc<LlmResponseCache>>,
}

impl CloudLlmClient {
    pub fn new(cfg: CloudLlmConfig) -> Result<Self, CloudLlmError> {
        let cache = if cfg.cache_enabled {
            if cfg.cache_max_entries == 0 {
                return Err(CloudLlmError::InvalidConfig(
                    "cache_max_entries must be >= 1".into(),
                ));
            }
            if cfg.cache_ttl == Duration::ZERO {
                return Err(CloudLlmError::InvalidConfig("cache_ttl must be > 0".into()));
            }

            Some(shared_cache(CacheSettings {
                max_entries: cfg.cache_max_entries,
                ttl: cfg.cache_ttl,
            }))
        } else {
            None
        };

        let http = reqwest::Client::builder()
            .timeout(cfg.timeout)
            .user_agent("nova-ai/0.1.0")
            .build()?;
        Ok(Self { cfg, http, cache })
    }

    pub fn config(&self) -> &CloudLlmConfig {
        &self.cfg
    }

    pub fn build_request_parts(
        &self,
        req: &GenerateRequest,
    ) -> Result<ProviderRequestParts, CloudLlmError> {
        match &self.cfg.provider {
            ProviderKind::OpenAi => {
                let url = join_url(&self.cfg.endpoint, "v1/chat/completions")?;
                let mut headers = HeaderMap::new();
                let key = self.cfg.api_key.as_deref().ok_or_else(|| {
                    CloudLlmError::InvalidConfig("OpenAI requires api_key".into())
                })?;
                headers.insert(
                    AUTHORIZATION,
                    HeaderValue::from_str(&format!("Bearer {key}"))
                        .map_err(|e| CloudLlmError::InvalidConfig(e.to_string()))?,
                );
                let body = json!({
                    "model": self.cfg.model,
                    "messages": [{"role":"user","content": req.prompt}],
                    "max_tokens": req.max_tokens,
                    "temperature": req.temperature,
                });
                Ok(ProviderRequestParts { url, headers, body })
            }
            ProviderKind::Anthropic => {
                let url = join_url(&self.cfg.endpoint, "v1/messages")?;
                let mut headers = HeaderMap::new();
                let key = self.cfg.api_key.as_deref().ok_or_else(|| {
                    CloudLlmError::InvalidConfig("Anthropic requires api_key".into())
                })?;
                headers.insert(
                    "x-api-key",
                    HeaderValue::from_str(key)
                        .map_err(|e| CloudLlmError::InvalidConfig(e.to_string()))?,
                );
                headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
                let body = json!({
                    "model": self.cfg.model,
                    "max_tokens": req.max_tokens,
                    "messages": [{"role":"user","content": req.prompt}],
                });
                Ok(ProviderRequestParts { url, headers, body })
            }
            ProviderKind::Gemini => {
                let key = self.cfg.api_key.as_deref().ok_or_else(|| {
                    CloudLlmError::InvalidConfig("Gemini requires api_key".into())
                })?;
                let mut url = join_url(
                    &self.cfg.endpoint,
                    &format!("v1beta/models/{}:generateContent", self.cfg.model),
                )?;
                url.query_pairs_mut().append_pair("key", key);
                let headers = HeaderMap::new();
                let body = json!({
                    "contents": [{"parts":[{"text": req.prompt}]}],
                    "generationConfig": {
                        "maxOutputTokens": req.max_tokens,
                        "temperature": req.temperature,
                    }
                });
                Ok(ProviderRequestParts { url, headers, body })
            }
            ProviderKind::AzureOpenAi {
                deployment,
                api_version,
            } => {
                let mut url = join_url(
                    &self.cfg.endpoint,
                    &format!("openai/deployments/{deployment}/chat/completions"),
                )?;
                url.query_pairs_mut()
                    .append_pair("api-version", api_version);
                let mut headers = HeaderMap::new();
                let key = self.cfg.api_key.as_deref().ok_or_else(|| {
                    CloudLlmError::InvalidConfig("Azure OpenAI requires api_key".into())
                })?;
                headers.insert(
                    "api-key",
                    HeaderValue::from_str(key)
                        .map_err(|e| CloudLlmError::InvalidConfig(e.to_string()))?,
                );
                let body = json!({
                    "messages": [{"role":"user","content": req.prompt}],
                    "max_tokens": req.max_tokens,
                    "temperature": req.temperature,
                });
                Ok(ProviderRequestParts { url, headers, body })
            }
            ProviderKind::Http => {
                let url = self.cfg.endpoint.clone();
                let mut headers = HeaderMap::new();
                if let Some(key) = self.cfg.api_key.as_deref() {
                    headers.insert(
                        AUTHORIZATION,
                        HeaderValue::from_str(&format!("Bearer {key}"))
                            .map_err(|e| CloudLlmError::InvalidConfig(e.to_string()))?,
                    );
                }
                let body = json!({
                    "model": self.cfg.model,
                    "prompt": req.prompt,
                    "max_tokens": req.max_tokens,
                    "temperature": req.temperature,
                });
                Ok(ProviderRequestParts { url, headers, body })
            }
        }
    }

    pub async fn generate(
        &self,
        mut req: GenerateRequest,
        cancel: CancellationToken,
    ) -> Result<String, CloudLlmError> {
        let provider = provider_label(&self.cfg.provider);
        let request_id = if self.cfg.audit_logging {
            audit::next_request_id()
        } else {
            0
        };

        if cancel.is_cancelled() {
            if self.cfg.audit_logging {
                audit::log_llm_error(
                    request_id,
                    provider,
                    &self.cfg.model,
                    "request cancelled",
                    Duration::ZERO,
                    /*retry_count=*/ 0,
                    /*stream=*/ false,
                );
            }
            return Err(CloudLlmError::Cancelled);
        }

        if self.cfg.audit_logging {
            req.prompt = audit::sanitize_prompt_for_audit(&req.prompt);
        }

        let cache_key = self
            .cache
            .as_ref()
            .map(|_| build_cache_key(&self.cfg, &req));
        if let (Some(cache), Some(key)) = (&self.cache, cache_key) {
            if let Some(hit) = cache.get(key).await {
                if self.cfg.audit_logging {
                    let safe_url = audit::sanitize_url_for_log(&self.cfg.endpoint);
                    audit::log_llm_request(
                        request_id,
                        provider,
                        &self.cfg.model,
                        &req.prompt,
                        Some(&safe_url),
                        /*attempt=*/ 0,
                        /*stream=*/ false,
                    );
                    audit::log_llm_response(
                        request_id,
                        provider,
                        &self.cfg.model,
                        &hit,
                        Duration::ZERO,
                        /*retry_count=*/ 0,
                        /*stream=*/ false,
                        /*chunk_count=*/ None,
                    );
                } else {
                    debug!(
                        provider = provider_label(&self.cfg.provider),
                        model = %self.cfg.model,
                        "llm cache hit"
                    );
                }
                return Ok(hit);
            }
        }

        let overall_started_at = Instant::now();
        let mut attempt = 0usize;

        loop {
            if cancel.is_cancelled() {
                if self.cfg.audit_logging {
                    audit::log_llm_error(
                        request_id,
                        provider,
                        &self.cfg.model,
                        "request cancelled",
                        overall_started_at.elapsed(),
                        attempt,
                        /*stream=*/ false,
                    );
                }
                return Err(CloudLlmError::Cancelled);
            }

            let parts = self.build_request_parts(&req)?;
            let safe_url = audit::sanitize_url_for_log(&parts.url);

            if self.cfg.audit_logging {
                audit::log_llm_request(
                    request_id,
                    provider,
                    &self.cfg.model,
                    &req.prompt,
                    Some(&safe_url),
                    attempt,
                    /*stream=*/ false,
                );
            } else {
                debug!(provider = provider, url = %safe_url, "llm request");
            }

            let request_builder = self
                .http
                .post(parts.url)
                .headers(parts.headers)
                .json(&parts.body);

            let response = tokio::select! {
                _ = cancel.cancelled() => {
                    if self.cfg.audit_logging {
                        audit::log_llm_error(
                            request_id,
                            provider,
                            &self.cfg.model,
                            "request cancelled",
                            overall_started_at.elapsed(),
                            attempt,
                            /*stream=*/ false,
                        );
                    }
                    return Err(CloudLlmError::Cancelled);
                }
                resp = request_builder.send() => resp,
            };
            let response = match response {
                Ok(resp) => resp,
                Err(err) => {
                    if self.cfg.audit_logging {
                        audit::log_llm_error(
                            request_id,
                            provider,
                            &self.cfg.model,
                            &format!("request error to {safe_url}: {err}"),
                            overall_started_at.elapsed(),
                            attempt,
                            /*stream=*/ false,
                        );
                    }
                    return Err(err.into());
                }
            };

            let status = response.status();
            let bytes = tokio::select! {
                _ = cancel.cancelled() => {
                    if self.cfg.audit_logging {
                        audit::log_llm_error(
                            request_id,
                            provider,
                            &self.cfg.model,
                            "request cancelled",
                            overall_started_at.elapsed(),
                            attempt,
                            /*stream=*/ false,
                        );
                    }
                    return Err(CloudLlmError::Cancelled);
                }
                b = response.bytes() => b,
            };
            let bytes = match bytes {
                Ok(bytes) => bytes,
                Err(err) => {
                    if self.cfg.audit_logging {
                        audit::log_llm_error(
                            request_id,
                            provider,
                            &self.cfg.model,
                            &format!("failed to read response bytes from {safe_url}: {err}"),
                            overall_started_at.elapsed(),
                            attempt,
                            /*stream=*/ false,
                        );
                    }
                    return Err(err.into());
                }
            };

            if !status.is_success() {
                let body = String::from_utf8_lossy(&bytes).to_string();
                if attempt < self.cfg.retry.max_retries && should_retry(status) {
                    attempt += 1;
                    warn!(
                        provider = ?self.cfg.provider,
                        status = %status,
                        attempt,
                        "llm request failed, retrying"
                    );
                    backoff_sleep(attempt, &self.cfg.retry, &cancel).await?;
                    continue;
                }
                if self.cfg.audit_logging {
                    audit::log_llm_error(
                        request_id,
                        provider,
                        &self.cfg.model,
                        &format!("bad status {status} from {safe_url}: {body}"),
                        overall_started_at.elapsed(),
                        attempt,
                        /*stream=*/ false,
                    );
                }
                return Err(CloudLlmError::BadStatus { status, body });
            }

            let completion = match parse_completion(&self.cfg.provider, &bytes) {
                Ok(completion) => completion,
                Err(err) => {
                    if self.cfg.audit_logging {
                        audit::log_llm_error(
                            request_id,
                            provider,
                            &self.cfg.model,
                            &format!("failed to parse response from {safe_url}: {err}"),
                            overall_started_at.elapsed(),
                            attempt,
                            /*stream=*/ false,
                        );
                    }
                    return Err(err);
                }
            };
            if self.cfg.audit_logging {
                audit::log_llm_response(
                    request_id,
                    provider,
                    &self.cfg.model,
                    &completion,
                    overall_started_at.elapsed(),
                    attempt,
                    /*stream=*/ false,
                    /*chunk_count=*/ None,
                );
            } else {
                debug!(provider = provider, "llm response");
            }

            if let (Some(cache), Some(key)) = (&self.cache, cache_key) {
                cache.insert(key, completion.clone()).await;
            }
            return Ok(completion);
        }
    }
}

fn provider_label(provider: &ProviderKind) -> &'static str {
    match provider {
        ProviderKind::OpenAi => "openai",
        ProviderKind::Anthropic => "anthropic",
        ProviderKind::Gemini => "gemini",
        ProviderKind::AzureOpenAi { .. } => "azure_openai",
        ProviderKind::Http => "http",
    }
}

fn build_cache_key(cfg: &CloudLlmConfig, req: &GenerateRequest) -> CacheKey {
    let mut builder = CacheKeyBuilder::new("cloud_generate_v1");
    match &cfg.provider {
        ProviderKind::OpenAi => builder.push_str("openai"),
        ProviderKind::Anthropic => builder.push_str("anthropic"),
        ProviderKind::Gemini => builder.push_str("gemini"),
        ProviderKind::AzureOpenAi {
            deployment,
            api_version,
        } => {
            builder.push_str("azure_openai");
            builder.push_str(deployment);
            builder.push_str(api_version);
        }
        ProviderKind::Http => builder.push_str("http"),
    }
    builder.push_str(cfg.endpoint.as_str());
    builder.push_str(&cfg.model);
    builder.push_u32(req.max_tokens);
    builder.push_u32(req.temperature.to_bits());
    builder.push_str(&req.prompt);
    builder.finish()
}

fn should_retry(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS
        || status == StatusCode::REQUEST_TIMEOUT
        || status.is_server_error()
}

async fn backoff_sleep(
    attempt: usize,
    cfg: &RetryConfig,
    cancel: &CancellationToken,
) -> Result<(), CloudLlmError> {
    let factor = 2u32.saturating_pow((attempt.saturating_sub(1)).min(16) as u32);
    let mut delay = cfg.initial_backoff.saturating_mul(factor);
    if delay > cfg.max_backoff {
        delay = cfg.max_backoff;
    }

    tokio::select! {
        _ = cancel.cancelled() => Err(CloudLlmError::Cancelled),
        _ = tokio::time::sleep(delay) => Ok(()),
    }
}

fn join_url(base: &Url, path: &str) -> Result<Url, CloudLlmError> {
    base.join(path)
        .map_err(|e| CloudLlmError::InvalidConfig(e.to_string()))
}

fn parse_completion(provider: &ProviderKind, bytes: &[u8]) -> Result<String, CloudLlmError> {
    match provider {
        ProviderKind::OpenAi | ProviderKind::AzureOpenAi { .. } => {
            #[derive(Deserialize)]
            struct OpenAiChatResponse {
                choices: Vec<Choice>,
            }
            #[derive(Deserialize)]
            struct Choice {
                message: Message,
            }
            #[derive(Deserialize)]
            struct Message {
                content: String,
            }

            let resp: OpenAiChatResponse = serde_json::from_slice(bytes)
                .map_err(|e| CloudLlmError::InvalidResponse(e.to_string()))?;
            resp.choices
                .into_iter()
                .next()
                .map(|c| c.message.content)
                .ok_or_else(|| CloudLlmError::InvalidResponse("missing choices[0]".into()))
        }
        ProviderKind::Anthropic => {
            #[derive(Deserialize)]
            struct AnthropicResponse {
                content: Vec<AnthropicContent>,
            }
            #[derive(Deserialize)]
            struct AnthropicContent {
                #[serde(default)]
                text: String,
            }

            let resp: AnthropicResponse = serde_json::from_slice(bytes)
                .map_err(|e| CloudLlmError::InvalidResponse(e.to_string()))?;
            resp.content
                .into_iter()
                .next()
                .map(|c| c.text)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| CloudLlmError::InvalidResponse("missing content[0].text".into()))
        }
        ProviderKind::Gemini => {
            #[derive(Deserialize)]
            struct GeminiResponse {
                candidates: Vec<Candidate>,
            }
            #[derive(Deserialize)]
            struct Candidate {
                content: GeminiContent,
            }
            #[derive(Deserialize)]
            struct GeminiContent {
                parts: Vec<Part>,
            }
            #[derive(Deserialize)]
            struct Part {
                #[serde(default)]
                text: String,
            }

            let resp: GeminiResponse = serde_json::from_slice(bytes)
                .map_err(|e| CloudLlmError::InvalidResponse(e.to_string()))?;
            resp.candidates
                .into_iter()
                .next()
                .and_then(|c| c.content.parts.into_iter().next())
                .map(|p| p.text)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    CloudLlmError::InvalidResponse(
                        "missing candidates[0].content.parts[0].text".into(),
                    )
                })
        }
        ProviderKind::Http => {
            #[derive(Deserialize)]
            struct HttpResponse {
                #[serde(default)]
                completion: String,
            }
            let resp: HttpResponse = serde_json::from_slice(bytes)
                .map_err(|e| CloudLlmError::InvalidResponse(e.to_string()))?;
            if resp.completion.is_empty() {
                return Err(CloudLlmError::InvalidResponse(
                    "missing completion field".into(),
                ));
            }
            Ok(resp.completion)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use std::collections::HashMap;
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

    fn audit_events(events: &[CapturedEvent]) -> Vec<CapturedEvent> {
        events
            .iter()
            .filter(|event| event.target == nova_config::AI_AUDIT_TARGET)
            .cloned()
            .collect()
    }

    #[test]
    fn builds_openai_request() {
        let cfg = CloudLlmConfig {
            provider: ProviderKind::OpenAi,
            endpoint: Url::parse("https://api.openai.com/").unwrap(),
            api_key: Some("test-key".to_string()),
            model: "gpt-4o-mini".to_string(),
            timeout: Duration::from_secs(1),
            retry: RetryConfig::default(),
            audit_logging: false,
            cache_enabled: false,
            cache_max_entries: 256,
            cache_ttl: Duration::from_secs(300),
        };

        let client = CloudLlmClient::new(cfg).unwrap();
        let parts = client
            .build_request_parts(&GenerateRequest {
                prompt: "Hello".to_string(),
                max_tokens: 10,
                temperature: 0.2,
            })
            .unwrap();

        assert!(parts.url.as_str().ends_with("/v1/chat/completions"));
        assert_eq!(parts.headers.get(AUTHORIZATION).unwrap(), "Bearer test-key");
        assert_eq!(parts.body["model"], "gpt-4o-mini");
        assert_eq!(parts.body["messages"][0]["content"], "Hello");
    }

    #[test]
    fn builds_anthropic_request() {
        let cfg = CloudLlmConfig {
            provider: ProviderKind::Anthropic,
            endpoint: Url::parse("https://api.anthropic.com/").unwrap(),
            api_key: Some("test-key".to_string()),
            model: "claude-3-5-sonnet-latest".to_string(),
            timeout: Duration::from_secs(1),
            retry: RetryConfig::default(),
            audit_logging: false,
            cache_enabled: false,
            cache_max_entries: 256,
            cache_ttl: Duration::from_secs(300),
        };

        let client = CloudLlmClient::new(cfg).unwrap();
        let parts = client
            .build_request_parts(&GenerateRequest {
                prompt: "Hello".to_string(),
                max_tokens: 10,
                temperature: 0.2,
            })
            .unwrap();

        assert!(parts.url.as_str().ends_with("/v1/messages"));
        assert_eq!(parts.headers.get("x-api-key").unwrap(), "test-key");
        assert_eq!(
            parts.headers.get("anthropic-version").unwrap(),
            "2023-06-01"
        );
        assert_eq!(parts.body["model"], "claude-3-5-sonnet-latest");
        assert_eq!(parts.body["messages"][0]["content"], "Hello");
    }

    #[test]
    fn builds_gemini_request() {
        let cfg = CloudLlmConfig {
            provider: ProviderKind::Gemini,
            endpoint: Url::parse("https://generativelanguage.googleapis.com/").unwrap(),
            api_key: Some("test-key".to_string()),
            model: "gemini-1.5-flash".to_string(),
            timeout: Duration::from_secs(1),
            retry: RetryConfig::default(),
            audit_logging: false,
            cache_enabled: false,
            cache_max_entries: 256,
            cache_ttl: Duration::from_secs(300),
        };

        let client = CloudLlmClient::new(cfg).unwrap();
        let parts = client
            .build_request_parts(&GenerateRequest {
                prompt: "Hello".to_string(),
                max_tokens: 10,
                temperature: 0.2,
            })
            .unwrap();

        assert!(parts
            .url
            .as_str()
            .contains("/v1beta/models/gemini-1.5-flash:generateContent"));
        assert!(parts.url.as_str().contains("key=test-key"));
        assert_eq!(parts.body["contents"][0]["parts"][0]["text"], "Hello");
    }

    #[test]
    fn builds_azure_openai_request() {
        let cfg = CloudLlmConfig {
            provider: ProviderKind::AzureOpenAi {
                deployment: "my-deployment".to_string(),
                api_version: "2024-02-01".to_string(),
            },
            endpoint: Url::parse("https://example.openai.azure.com/").unwrap(),
            api_key: Some("test-key".to_string()),
            model: "unused".to_string(),
            timeout: Duration::from_secs(1),
            retry: RetryConfig::default(),
            audit_logging: false,
            cache_enabled: false,
            cache_max_entries: 256,
            cache_ttl: Duration::from_secs(300),
        };

        let client = CloudLlmClient::new(cfg).unwrap();
        let parts = client
            .build_request_parts(&GenerateRequest {
                prompt: "Hello".to_string(),
                max_tokens: 10,
                temperature: 0.2,
            })
            .unwrap();

        assert!(parts
            .url
            .as_str()
            .contains("/openai/deployments/my-deployment/chat/completions?api-version=2024-02-01"));
        assert_eq!(parts.headers.get("api-key").unwrap(), "test-key");
        assert_eq!(parts.body["messages"][0]["content"], "Hello");
    }

    #[tokio::test]
    async fn http_provider_works_against_mock_server() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST).path("/complete");
            then.status(200).json_body(json!({ "completion": "Pong" }));
        });

        let cfg = CloudLlmConfig {
            provider: ProviderKind::Http,
            endpoint: Url::parse(&format!("{}/complete", server.base_url())).unwrap(),
            api_key: None,
            model: "default".to_string(),
            timeout: Duration::from_secs(1),
            retry: RetryConfig {
                max_retries: 0,
                ..RetryConfig::default()
            },
            audit_logging: false,
            cache_enabled: false,
            cache_max_entries: 256,
            cache_ttl: Duration::from_secs(300),
        };

        let client = CloudLlmClient::new(cfg).unwrap();
        let out = client
            .generate(
                GenerateRequest {
                    prompt: "Ping".to_string(),
                    max_tokens: 5,
                    temperature: 0.2,
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();

        mock.assert();
        assert_eq!(out, "Pong");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn audit_logging_emits_sanitized_events_and_strips_query_params() {
        let server = MockServer::start();

        let secret = "sk-proj-012345678901234567890123456789";
        let query_secret = "supersecret";
        let endpoint = Url::parse(&format!(
            "{}/complete?token={query_secret}",
            server.base_url()
        ))
        .unwrap();

        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/complete")
                .query_param("token", query_secret)
                .body_contains("[REDACTED]");
            then.status(200)
                .json_body(json!({ "completion": format!("Pong {secret}") }));
        });

        let events = Arc::new(Mutex::new(Vec::<CapturedEvent>::new()));
        let layer = CapturingLayer {
            events: events.clone(),
        };

        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        let cfg = CloudLlmConfig {
            provider: ProviderKind::Http,
            endpoint,
            api_key: Some("test-api-key".to_string()),
            model: "default".to_string(),
            timeout: Duration::from_secs(1),
            retry: RetryConfig {
                max_retries: 0,
                ..RetryConfig::default()
            },
            audit_logging: true,
            cache_enabled: false,
            cache_max_entries: 256,
            cache_ttl: Duration::from_secs(300),
        };

        let client = CloudLlmClient::new(cfg).unwrap();
        let out = client
            .generate(
                GenerateRequest {
                    prompt: format!("hello {secret}"),
                    max_tokens: 5,
                    temperature: 0.2,
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();

        mock.assert();
        assert!(
            out.contains(secret),
            "client returns provider output unchanged"
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
            .expect("request_id field present");
        let prompt = request.fields.get("prompt").expect("prompt field present");
        assert!(prompt.contains("[REDACTED]"));
        assert!(!prompt.contains(secret));

        let endpoint = request
            .fields
            .get("endpoint")
            .expect("endpoint field present");
        assert!(!endpoint.contains(query_secret));
        assert!(!endpoint.contains("token="));
        assert!(!endpoint.contains('?'));

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
        assert!(completion.contains("[REDACTED]"));
        assert!(!completion.contains(secret));

        for event in audit {
            for value in event.fields.values() {
                assert!(!value.contains("test-api-key"));
                assert!(!value.contains(query_secret));
            }
        }
    }
}
