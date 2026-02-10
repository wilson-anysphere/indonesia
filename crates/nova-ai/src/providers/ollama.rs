use crate::{
    http::map_reqwest_error,
    providers::LlmProvider,
    stream_decode::{
        ensure_max_stream_frame_size, trim_ascii_whitespace, MAX_STREAM_FRAME_BYTES,
    },
    types::{AiStream, ChatMessage, ChatRequest},
    AiError,
};
use async_stream::try_stream;
use async_trait::async_trait;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use url::Url;

#[derive(Clone)]
pub struct OllamaProvider {
    base_url: Url,
    model: String,
    timeout: Duration,
    client: reqwest::Client,
}

impl OllamaProvider {
    pub fn new(
        base_url: Url,
        model: impl Into<String>,
        timeout: Duration,
    ) -> Result<Self, AiError> {
        let client = reqwest::Client::builder().build()?;
        Ok(Self {
            base_url,
            model: model.into(),
            timeout,
            client,
        })
    }

    fn endpoint(&self, path: &str) -> Result<Url, AiError> {
        let base_str = self.base_url.as_str().trim_end_matches('/').to_string();
        let base = Url::parse(&format!("{base_str}/"))?;
        let base_path = base.path().trim_end_matches('/');
        let mut relative = path.trim_start_matches('/');
        if base_path.ends_with("/api") && relative.starts_with("api/") {
            relative = relative.trim_start_matches("api/");
        }
        Ok(base.join(relative)?)
    }
}

#[async_trait]
impl LlmProvider for OllamaProvider {
    async fn chat(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<String, AiError> {
        let url = self.endpoint("/api/chat")?;
        let options = if request.max_tokens.is_some() || request.temperature.is_some() {
            Some(OllamaOptions {
                num_predict: request.max_tokens,
                temperature: request.temperature,
            })
        } else {
            None
        };
        let body = OllamaChatRequest {
            model: &self.model,
            messages: &request.messages,
            stream: false,
            options,
        };

        let fut = async {
            let response = self
                .client
                .post(url)
                .json(&body)
                .timeout(self.timeout)
                .send()
                .await
                .map_err(map_reqwest_error)?
                .error_for_status()
                .map_err(map_reqwest_error)?;

            let parsed: OllamaChatResponse = response.json().await.map_err(map_reqwest_error)?;
            let content = parsed.message.map(|m| m.content).unwrap_or_default();
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
        let url = self.endpoint("/api/chat")?;
        let options = if request.max_tokens.is_some() || request.temperature.is_some() {
            Some(OllamaOptions {
                num_predict: request.max_tokens,
                temperature: request.temperature,
            })
        } else {
            None
        };
        let body = OllamaChatRequest {
            model: &self.model,
            messages: &request.messages,
            stream: true,
            options,
        };

        let response = tokio::select! {
            _ = cancel.cancelled() => return Err(AiError::Cancelled),
            resp = self
                .client
                .post(url)
                .json(&body)
                .timeout(self.timeout)
                .send() => resp.map_err(map_reqwest_error)?,
        }
        .error_for_status()
        .map_err(map_reqwest_error)?;

        let mut bytes_stream = response.bytes_stream();
        let timeout = self.timeout;

        let stream = try_stream! {
            // Ollama streams newline-delimited JSON objects. We must buffer raw bytes until a full
            // `\n`-terminated line is available to avoid corrupting multibyte UTF-8 sequences that
            // may be split across network chunks.
            let mut buffer: Vec<u8> = Vec::new();
            let mut cursor: usize = 0;

            loop {
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
                // Validate the next chunk before buffering it, using the amount of data already
                // buffered for the current (incomplete) JSONL frame.
                ensure_max_stream_frame_size(
                    buffer.len().saturating_sub(cursor),
                    chunk.as_ref(),
                    MAX_STREAM_FRAME_BYTES,
                )?;
                buffer.extend_from_slice(&chunk);

                while let Some(rel_pos) = buffer[cursor..].iter().position(|&b| b == b'\n') {
                    let line_end = cursor + rel_pos;
                    let mut line_bytes = &buffer[cursor..line_end];
                    cursor = line_end + 1;

                    // Handle CRLF line endings.
                    if let Some(stripped) = line_bytes.strip_suffix(b"\r") {
                        line_bytes = stripped;
                    }
                    let line_bytes = trim_ascii_whitespace(line_bytes);
                    if line_bytes.is_empty() {
                        continue;
                    }

                    let parsed: OllamaChatResponse = serde_json::from_slice(line_bytes)?;
                    if let Some(message) = parsed.message {
                        if !message.content.is_empty() {
                            yield message.content;
                        }
                    }
                    if parsed.done {
                        return;
                    }
                }

                // If we've consumed a significant prefix of the buffer, compact it in-place to
                // avoid unbounded growth while still preventing quadratic copying.
                if cursor == buffer.len() {
                    buffer.clear();
                    cursor = 0;
                } else if cursor >= 8 * 1024 && cursor >= buffer.len() / 2 {
                    buffer.drain(..cursor);
                    cursor = 0;
                }
            }
        };

        let stream: AiStream = Box::pin(stream);
        Ok(stream)
    }

    async fn list_models(&self, cancel: CancellationToken) -> Result<Vec<String>, AiError> {
        let url = self.endpoint("/api/tags")?;
        let fut = async {
            let response = self
                .client
                .get(url)
                .timeout(self.timeout)
                .send()
                .await
                .map_err(map_reqwest_error)?
                .error_for_status()
                .map_err(map_reqwest_error)?;

            let parsed: OllamaTagsResponse = response.json().await.map_err(map_reqwest_error)?;
            Ok::<_, AiError>(parsed.models.into_iter().map(|m| m.name).collect())
        };

        tokio::select! {
            _ = cancel.cancelled() => Err(AiError::Cancelled),
            res = fut => res,
        }
    }
}

#[derive(Debug, Serialize)]
struct OllamaChatRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<OllamaOptions>,
}

#[derive(Debug, Serialize)]
struct OllamaOptions {
    #[serde(rename = "num_predict", skip_serializing_if = "Option::is_none")]
    num_predict: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
}

#[derive(Debug, Deserialize)]
struct OllamaChatResponse {
    #[serde(default)]
    message: Option<OllamaMessage>,
    #[serde(default)]
    done: bool,
}

#[derive(Debug, Deserialize)]
struct OllamaMessage {
    #[serde(default)]
    content: String,
}

#[derive(Debug, Deserialize)]
struct OllamaTagsResponse {
    #[serde(default)]
    models: Vec<OllamaTagModel>,
}

#[derive(Debug, Deserialize)]
struct OllamaTagModel {
    name: String,
}
