use crate::{
    http::map_reqwest_error,
    providers::LlmProvider,
    types::{AiStream, ChatMessage, ChatRequest},
    AiError,
};
use async_stream::try_stream;
use async_trait::async_trait;
use bytes::BytesMut;
use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use url::Url;

#[derive(Clone)]
pub struct OpenAiCompatibleProvider {
    base_url: Url,
    model: String,
    timeout: Duration,
    api_key: Option<String>,
    client: reqwest::Client,
}

impl OpenAiCompatibleProvider {
    pub fn new(
        base_url: Url,
        model: impl Into<String>,
        timeout: Duration,
        api_key: Option<String>,
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
            base_url,
            model: model.into(),
            timeout,
            api_key,
            client,
        })
    }

    fn authorize(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(api_key) => request.bearer_auth(api_key),
            None => request,
        }
    }

    fn endpoint(&self, path: &str) -> Result<Url, AiError> {
        // Accept both:
        // - http://localhost:8000  (we will append /v1/...)
        // - http://localhost:8000/v1  (we will append /...)
        let mut base = self.base_url.clone();
        let base_str = base.as_str().trim_end_matches('/').to_string();
        base = Url::parse(&format!("{base_str}/"))?;

        let base_path = base.path().trim_end_matches('/');
        if base_path.ends_with("/v1") {
            Ok(base.join(path.trim_start_matches('/'))?)
        } else {
            Ok(base.join(&format!("v1/{}", path.trim_start_matches('/')))?)
        }
    }
}

#[async_trait]
impl LlmProvider for OpenAiCompatibleProvider {
    async fn chat(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<String, AiError> {
        let url = self.endpoint("/chat/completions")?;
        let body = OpenAiChatCompletionRequest {
            model: &self.model,
            messages: &request.messages,
            max_tokens: request.max_tokens,
            temperature: request.temperature,
            stream: false,
        };

        let fut = async {
            let response = self
                .authorize(self.client.post(url))
                .json(&body)
                .timeout(self.timeout)
                .send()
                .await
                .map_err(map_reqwest_error)?
                .error_for_status()
                .map_err(map_reqwest_error)?;

            let parsed: OpenAiChatCompletionResponse =
                response.json().await.map_err(map_reqwest_error)?;
            let content = parsed
                .choices
                .into_iter()
                .next()
                .and_then(|choice| choice.message.content)
                .ok_or_else(|| {
                    AiError::UnexpectedResponse("missing choices[0].message.content".into())
                })?;
            Ok::<_, AiError>(content)
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
        let url = self.endpoint("/chat/completions")?;
        let body = OpenAiChatCompletionRequest {
            model: &self.model,
            messages: &request.messages,
            max_tokens: request.max_tokens,
            temperature: request.temperature,
            stream: true,
        };

        let request_builder = self
            .authorize(self.client.post(url))
            .json(&body)
            .timeout(self.timeout);

        let response = tokio::select! {
            _ = cancel.cancelled() => return Err(AiError::Cancelled),
            resp = request_builder.send() => resp.map_err(map_reqwest_error)?,
        }
        .error_for_status()
        .map_err(map_reqwest_error)?;

        let mut bytes_stream = response.bytes_stream();
        let timeout = self.timeout;

        let stream = try_stream! {
            let mut buffer = BytesMut::new();
            // Index of the next byte we haven't scanned yet for `\n`. This avoids rescanning the
            // same prefix repeatedly when the transport splits a single SSE line across many
            // small chunks.
            let mut scan_start: usize = 0;

            loop {
                // Drain all complete lines already buffered before waiting for more bytes.
                while let Some(rel_pos) = buffer[scan_start..].iter().position(|&b| b == b'\n') {
                    let newline_pos = scan_start + rel_pos;
                    // Split off the line (including the trailing '\n') so `buffer` stays valid.
                    let mut line = buffer.split_to(newline_pos + 1);
                    // `split_to` advances the start of `buffer`, so any previous scan offset is no
                    // longer meaningful.
                    scan_start = 0;
                    // Drop the trailing '\n'.
                    line.truncate(newline_pos);
                    // Handle CRLF line endings.
                    if line.last() == Some(&b'\r') {
                        line.truncate(line.len() - 1);
                    }

                    let line = std::str::from_utf8(&line).map_err(|e| {
                        AiError::UnexpectedResponse(format!("invalid UTF-8 in SSE stream: {e}"))
                    })?;

                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }

                    let Some(data) = line.strip_prefix("data:") else {
                        continue;
                    };
                    let data = data.trim();
                    if data == "[DONE]" {
                        return;
                    }

                    let parsed: OpenAiChatCompletionStreamResponse = serde_json::from_str(data)?;
                    for choice in parsed.choices {
                        if let Some(content) = choice.delta.content {
                            if !content.is_empty() {
                                yield content;
                            }
                        }
                    }
                }
                // We've scanned everything currently in the buffer without finding a newline.
                scan_start = buffer.len();

                let next = tokio::select! {
                    _ = cancel.cancelled() => Err(AiError::Cancelled),
                    chunk = tokio::time::timeout(timeout, bytes_stream.next()) => {
                        match chunk {
                            Ok(item) => Ok(item),
                            Err(_) => Err(AiError::Timeout),
                        }
                    }
                }?;

                let Some(chunk) = next else { break };
                let chunk = chunk.map_err(map_reqwest_error)?;
                buffer.extend_from_slice(&chunk);
            }
        };

        let stream: AiStream = Box::pin(stream);
        Ok(stream)
    }

    async fn list_models(&self, cancel: CancellationToken) -> Result<Vec<String>, AiError> {
        let url = self.endpoint("/models")?;
        let fut = async {
            let response = self
                .authorize(self.client.get(url))
                .timeout(self.timeout)
                .send()
                .await
                .map_err(map_reqwest_error)?
                .error_for_status()
                .map_err(map_reqwest_error)?;
            let parsed: OpenAiModelsResponse = response.json().await.map_err(map_reqwest_error)?;
            Ok::<_, AiError>(parsed.data.into_iter().map(|model| model.id).collect())
        };

        tokio::select! {
            _ = cancel.cancelled() => Err(AiError::Cancelled),
            res = fut => res,
        }
    }
}

#[derive(Debug, Serialize)]
struct OpenAiChatCompletionRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    stream: bool,
}

#[derive(Debug, Deserialize)]
struct OpenAiChatCompletionResponse {
    choices: Vec<OpenAiChatCompletionChoice>,
}

#[derive(Debug, Deserialize)]
struct OpenAiChatCompletionChoice {
    message: OpenAiChatCompletionMessage,
}

#[derive(Debug, Deserialize)]
struct OpenAiChatCompletionMessage {
    content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAiChatCompletionStreamResponse {
    choices: Vec<OpenAiChatCompletionStreamChoice>,
}

#[derive(Debug, Deserialize)]
struct OpenAiChatCompletionStreamChoice {
    delta: OpenAiChatCompletionStreamDelta,
}

#[derive(Debug, Deserialize)]
struct OpenAiChatCompletionStreamDelta {
    content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAiModelsResponse {
    data: Vec<OpenAiModelInfo>,
}

#[derive(Debug, Deserialize)]
struct OpenAiModelInfo {
    id: String,
}
