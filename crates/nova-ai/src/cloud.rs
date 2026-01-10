use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use url::Url;

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
    AzureOpenAi { deployment: String, api_version: String },
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
}

impl CloudLlmClient {
    pub fn new(cfg: CloudLlmConfig) -> Result<Self, CloudLlmError> {
        let http = reqwest::Client::builder()
            .timeout(cfg.timeout)
            .user_agent("nova-ai/0.1.0")
            .build()?;
        Ok(Self { cfg, http })
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
                let key = self
                    .cfg
                    .api_key
                    .as_deref()
                    .ok_or_else(|| CloudLlmError::InvalidConfig("OpenAI requires api_key".into()))?;
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
                headers.insert(
                    "anthropic-version",
                    HeaderValue::from_static("2023-06-01"),
                );
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
        req: GenerateRequest,
        cancel: CancellationToken,
    ) -> Result<String, CloudLlmError> {
        let mut attempt = 0usize;

        loop {
            if cancel.is_cancelled() {
                return Err(CloudLlmError::Cancelled);
            }

            let parts = self.build_request_parts(&req)?;
            if self.cfg.audit_logging {
                info!(
                    provider = ?self.cfg.provider,
                    url = %parts.url,
                    prompt = %req.prompt,
                    "llm request"
                );
            } else {
                debug!(provider = ?self.cfg.provider, url = %parts.url, "llm request");
            }

            let request_builder = self.http.post(parts.url).headers(parts.headers).json(&parts.body);

            let response = tokio::select! {
                _ = cancel.cancelled() => return Err(CloudLlmError::Cancelled),
                resp = request_builder.send() => resp?,
            };

            let status = response.status();
            let bytes = tokio::select! {
                _ = cancel.cancelled() => return Err(CloudLlmError::Cancelled),
                b = response.bytes() => b?,
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
                return Err(CloudLlmError::BadStatus { status, body });
            }

            let completion = parse_completion(&self.cfg.provider, &bytes)?;
            if self.cfg.audit_logging {
                info!(
                    provider = ?self.cfg.provider,
                    completion = %completion,
                    "llm response"
                );
            } else {
                debug!(provider = ?self.cfg.provider, "llm response");
            }
            return Ok(completion);
        }
    }
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
                .ok_or_else(|| CloudLlmError::InvalidResponse("missing candidates[0].content.parts[0].text".into()))
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
        assert_eq!(
            parts.headers.get(AUTHORIZATION).unwrap(),
            "Bearer test-key"
        );
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
        };

        let client = CloudLlmClient::new(cfg).unwrap();
        let parts = client
            .build_request_parts(&GenerateRequest {
                prompt: "Hello".to_string(),
                max_tokens: 10,
                temperature: 0.2,
            })
            .unwrap();

        assert!(parts.url.as_str().contains(
            "/openai/deployments/my-deployment/chat/completions?api-version=2024-02-01"
        ));
        assert_eq!(parts.headers.get("api-key").unwrap(), "test-key");
        assert_eq!(parts.body["messages"][0]["content"], "Hello");
    }

    #[tokio::test]
    async fn http_provider_works_against_mock_server() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST).path("/complete");
            then.status(200)
                .json_body(json!({ "completion": "Pong" }));
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
}
