use crate::providers::LlmProvider;
use crate::types::{AiStream, ChatMessage, ChatRequest, ChatRole};
use crate::AiError;
use async_stream::try_stream;
use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use url::Url;

/// Anthropic Messages API provider.
#[derive(Clone)]
pub(crate) struct AnthropicProvider {
    endpoint: Url,
    model: String,
    timeout: Duration,
    client: reqwest::Client,
}

impl AnthropicProvider {
    pub(crate) fn new(
        endpoint: Url,
        api_key: String,
        model: impl Into<String>,
        timeout: Duration,
    ) -> Result<Self, AiError> {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-api-key",
            HeaderValue::from_str(&api_key)
                .map_err(|e| AiError::InvalidConfig(format!("invalid anthropic api_key: {e}")))?,
        );
        headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .build()?;

        Ok(Self {
            endpoint,
            model: model.into(),
            timeout,
            client,
        })
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    async fn chat(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<String, AiError> {
        let url = self
            .endpoint
            .join("v1/messages")
            .map_err(|e| AiError::InvalidConfig(e.to_string()))?;

        let (system, messages) = anthropic_messages(&request.messages);

        let mut body = serde_json::Map::new();
        body.insert("model".to_string(), json!(self.model));
        body.insert(
            "max_tokens".to_string(),
            json!(request.max_tokens.unwrap_or(1024)),
        );
        body.insert("messages".to_string(), json!(messages));
        if !system.trim().is_empty() {
            body.insert("system".to_string(), json!(system));
        }
        if let Some(temp) = request.temperature {
            body.insert("temperature".to_string(), json!(temp));
        }

        let fut = async {
            let response = self
                .client
                .post(url)
                .json(&body)
                .timeout(self.timeout)
                .send()
                .await?
                .error_for_status()?;

            let bytes = response.bytes().await?;
            parse_anthropic_completion(&bytes)
        };

        tokio::select! {
            _ = cancel.cancelled() => Err(AiError::Cancelled),
            res = fut => res,
        }
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<AiStream, AiError> {
        let out = self.chat(request, cancel).await?;
        let stream = try_stream! {
            yield out;
        };
        Ok(Box::pin(stream))
    }

    async fn list_models(&self, _cancel: CancellationToken) -> Result<Vec<String>, AiError> {
        // Anthropic does not offer a stable "list models" endpoint. Return the configured one.
        Ok(vec![self.model.clone()])
    }
}

/// Gemini (Generative Language) provider.
#[derive(Clone)]
pub(crate) struct GeminiProvider {
    endpoint: Url,
    api_key: String,
    model: String,
    timeout: Duration,
    client: reqwest::Client,
}

impl GeminiProvider {
    pub(crate) fn new(
        endpoint: Url,
        api_key: String,
        model: impl Into<String>,
        timeout: Duration,
    ) -> Result<Self, AiError> {
        let client = reqwest::Client::builder().build()?;
        Ok(Self {
            endpoint,
            api_key,
            model: model.into(),
            timeout,
            client,
        })
    }
}

#[async_trait]
impl LlmProvider for GeminiProvider {
    async fn chat(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<String, AiError> {
        let mut url = self
            .endpoint
            .join(&format!("v1beta/models/{}:generateContent", self.model))
            .map_err(|e| AiError::InvalidConfig(e.to_string()))?;
        url.query_pairs_mut().append_pair("key", &self.api_key);

        let prompt = messages_to_prompt(&request.messages);

        let mut generation_config = serde_json::Map::new();
        generation_config.insert(
            "maxOutputTokens".to_string(),
            json!(request.max_tokens.unwrap_or(1024)),
        );
        if let Some(temp) = request.temperature {
            generation_config.insert("temperature".to_string(), json!(temp));
        }

        let body = json!({
            "contents": [{"parts":[{"text": prompt}]}],
            "generationConfig": generation_config,
        });

        let fut = async {
            let response = self
                .client
                .post(url)
                .json(&body)
                .timeout(self.timeout)
                .send()
                .await?
                .error_for_status()?;

            let bytes = response.bytes().await?;
            parse_gemini_completion(&bytes)
        };

        tokio::select! {
            _ = cancel.cancelled() => Err(AiError::Cancelled),
            res = fut => res,
        }
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<AiStream, AiError> {
        let out = self.chat(request, cancel).await?;
        let stream = try_stream! {
            yield out;
        };
        Ok(Box::pin(stream))
    }

    async fn list_models(&self, _cancel: CancellationToken) -> Result<Vec<String>, AiError> {
        // Gemini model discovery requires a different API surface; return the configured one.
        Ok(vec![self.model.clone()])
    }
}

/// Azure OpenAI chat-completions provider.
#[derive(Clone)]
pub(crate) struct AzureOpenAiProvider {
    endpoint: Url,
    deployment: String,
    api_version: String,
    timeout: Duration,
    client: reqwest::Client,
}

impl AzureOpenAiProvider {
    pub(crate) fn new(
        endpoint: Url,
        api_key: String,
        deployment: String,
        api_version: String,
        timeout: Duration,
    ) -> Result<Self, AiError> {
        let mut headers = HeaderMap::new();
        headers.insert(
            "api-key",
            HeaderValue::from_str(&api_key)
                .map_err(|e| AiError::InvalidConfig(format!("invalid azure api_key: {e}")))?,
        );
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .build()?;
        Ok(Self {
            endpoint,
            deployment,
            api_version,
            timeout,
            client,
        })
    }
}

#[async_trait]
impl LlmProvider for AzureOpenAiProvider {
    async fn chat(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<String, AiError> {
        let mut url = self
            .endpoint
            .join(&format!(
                "openai/deployments/{}/chat/completions",
                self.deployment
            ))
            .map_err(|e| AiError::InvalidConfig(e.to_string()))?;
        url.query_pairs_mut()
            .append_pair("api-version", &self.api_version);

        let mut body = serde_json::Map::new();
        body.insert("messages".to_string(), json!(request.messages));
        body.insert(
            "max_tokens".to_string(),
            json!(request.max_tokens.unwrap_or(1024)),
        );
        if let Some(temp) = request.temperature {
            body.insert("temperature".to_string(), json!(temp));
        }

        let fut = async {
            let response = self
                .client
                .post(url)
                .json(&body)
                .timeout(self.timeout)
                .send()
                .await?
                .error_for_status()?;
            let bytes = response.bytes().await?;
            parse_openai_completion(&bytes)
        };

        tokio::select! {
            _ = cancel.cancelled() => Err(AiError::Cancelled),
            res = fut => res,
        }
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<AiStream, AiError> {
        let out = self.chat(request, cancel).await?;
        let stream = try_stream! {
            yield out;
        };
        Ok(Box::pin(stream))
    }

    async fn list_models(&self, _cancel: CancellationToken) -> Result<Vec<String>, AiError> {
        Ok(vec![self.deployment.clone()])
    }
}

/// A minimal JSON-over-HTTP provider.
#[derive(Clone)]
pub(crate) struct HttpProvider {
    endpoint: Url,
    model: String,
    timeout: Duration,
    client: reqwest::Client,
}

impl HttpProvider {
    pub(crate) fn new(
        endpoint: Url,
        api_key: Option<String>,
        model: impl Into<String>,
        timeout: Duration,
    ) -> Result<Self, AiError> {
        let mut headers = HeaderMap::new();
        if let Some(key) = api_key.as_deref() {
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {key}"))
                    .map_err(|e| AiError::InvalidConfig(e.to_string()))?,
            );
        }
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .build()?;
        Ok(Self {
            endpoint,
            model: model.into(),
            timeout,
            client,
        })
    }
}

#[async_trait]
impl LlmProvider for HttpProvider {
    async fn chat(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<String, AiError> {
        let prompt = messages_to_prompt(&request.messages);

        let mut body = serde_json::Map::new();
        body.insert("model".to_string(), json!(self.model));
        body.insert("prompt".to_string(), json!(prompt));
        body.insert(
            "max_tokens".to_string(),
            json!(request.max_tokens.unwrap_or(1024)),
        );
        if let Some(temp) = request.temperature {
            body.insert("temperature".to_string(), json!(temp));
        }

        let fut = async {
            let response = self
                .client
                .post(self.endpoint.clone())
                .json(&body)
                .timeout(self.timeout)
                .send()
                .await?
                .error_for_status()?;
            let bytes = response.bytes().await?;
            parse_http_completion(&bytes)
        };

        tokio::select! {
            _ = cancel.cancelled() => Err(AiError::Cancelled),
            res = fut => res,
        }
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<AiStream, AiError> {
        let out = self.chat(request, cancel).await?;
        let stream = try_stream! {
            yield out;
        };
        Ok(Box::pin(stream))
    }

    async fn list_models(&self, _cancel: CancellationToken) -> Result<Vec<String>, AiError> {
        Ok(vec![self.model.clone()])
    }
}

fn messages_to_prompt(messages: &[ChatMessage]) -> String {
    let mut out = String::new();
    for msg in messages {
        let role = match msg.role {
            ChatRole::System => "System",
            ChatRole::User => "User",
            ChatRole::Assistant => "Assistant",
        };
        out.push_str(role);
        out.push_str(":\n");
        out.push_str(&msg.content);
        out.push_str("\n\n");
    }
    out.trim_end().to_string()
}

fn anthropic_messages(messages: &[ChatMessage]) -> (String, Vec<serde_json::Value>) {
    let system = messages
        .iter()
        .filter(|m| m.role == ChatRole::System)
        .map(|m| m.content.trim())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");

    let converted = messages
        .iter()
        .filter(|m| m.role != ChatRole::System)
        .map(|m| {
            let role = match m.role {
                ChatRole::User => "user",
                ChatRole::Assistant => "assistant",
                ChatRole::System => unreachable!("filtered"),
            };
            json!({
                "role": role,
                "content": m.content,
            })
        })
        .collect::<Vec<_>>();

    if converted.is_empty() {
        // Anthropic requires at least one user message. Fall back to the combined prompt.
        let prompt = messages_to_prompt(messages);
        return (system, vec![json!({"role":"user","content": prompt})]);
    }

    (system, converted)
}

fn parse_openai_completion(bytes: &[u8]) -> Result<String, AiError> {
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
        #[serde(default)]
        content: Option<String>,
    }

    let resp: OpenAiChatResponse = serde_json::from_slice(bytes)?;
    resp.choices
        .into_iter()
        .next()
        .and_then(|c| c.message.content)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AiError::UnexpectedResponse("missing choices[0].message.content".into()))
}

fn parse_anthropic_completion(bytes: &[u8]) -> Result<String, AiError> {
    #[derive(Deserialize)]
    struct AnthropicResponse {
        content: Vec<AnthropicContent>,
    }
    #[derive(Deserialize)]
    struct AnthropicContent {
        #[serde(default)]
        text: String,
    }

    let resp: AnthropicResponse = serde_json::from_slice(bytes)?;
    let mut out = String::new();
    for item in resp.content {
        if !item.text.is_empty() {
            out.push_str(&item.text);
        }
    }

    if out.is_empty() {
        return Err(AiError::UnexpectedResponse("missing content[*].text".into()));
    }

    Ok(out)
}

fn parse_gemini_completion(bytes: &[u8]) -> Result<String, AiError> {
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

    let resp: GeminiResponse = serde_json::from_slice(bytes)?;
    let mut out = String::new();

    let candidate = resp.candidates.into_iter().next().ok_or_else(|| {
        AiError::UnexpectedResponse("missing candidates[0].content.parts[*].text".into())
    })?;

    for part in candidate.content.parts {
        if !part.text.is_empty() {
            out.push_str(&part.text);
        }
    }

    if out.is_empty() {
        return Err(AiError::UnexpectedResponse(
            "missing candidates[0].content.parts[*].text".into(),
        ));
    }

    Ok(out)
}

fn parse_http_completion(bytes: &[u8]) -> Result<String, AiError> {
    #[derive(Deserialize)]
    struct HttpResponse {
        #[serde(default)]
        completion: String,
    }
    let resp: HttpResponse = serde_json::from_slice(bytes)?;
    if resp.completion.is_empty() {
        return Err(AiError::UnexpectedResponse(
            "missing completion field".into(),
        ));
    }
    Ok(resp.completion)
}
