use crate::{
    providers::LlmProvider,
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
        Ok(base.join(path.trim_start_matches('/'))?)
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
                .await?
                .error_for_status()?;

            let parsed: OllamaChatResponse = response.json().await?;
            let Some(message) = parsed.message else {
                return Err(AiError::UnexpectedResponse(
                    "missing message in Ollama chat response".into(),
                ));
            };
            Ok::<_, AiError>(message.content)
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
                .send() => resp?,
        }
        .error_for_status()?;

        let mut bytes_stream = response.bytes_stream();
        let timeout = self.timeout;

        let stream = try_stream! {
            let mut buffer = String::new();

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
                let chunk = chunk.map_err(AiError::Http)?;
                buffer.push_str(&String::from_utf8_lossy(&chunk));

                while let Some(pos) = buffer.find('\n') {
                    let line = buffer[..pos].to_string();
                    buffer = buffer[pos + 1..].to_string();
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }

                    let parsed: OllamaChatResponse = serde_json::from_str(line)?;
                    if let Some(message) = parsed.message {
                        if !message.content.is_empty() {
                            yield message.content;
                        }
                    }
                    if parsed.done {
                        return;
                    }
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
                .await?
                .error_for_status()?;

            let parsed: OllamaTagsResponse = response.json().await?;
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
