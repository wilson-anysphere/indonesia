use crate::{
    providers::AiProvider,
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
pub struct OpenAiCompatibleProvider {
    base_url: Url,
    model: String,
    timeout: Duration,
    client: reqwest::Client,
}

impl OpenAiCompatibleProvider {
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
impl AiProvider for OpenAiCompatibleProvider {
    async fn chat(
        &self,
        mut request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<String, AiError> {
        let url = self.endpoint("/chat/completions")?;
        let body = OpenAiChatCompletionRequest {
            model: &self.model,
            messages: &request.messages,
            max_tokens: request.max_tokens.take(),
            stream: false,
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

            let parsed: OpenAiChatCompletionResponse = response.json().await?;
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
        mut request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<AiStream, AiError> {
        let url = self.endpoint("/chat/completions")?;
        let body = OpenAiChatCompletionRequest {
            model: &self.model,
            messages: &request.messages,
            max_tokens: request.max_tokens.take(),
            stream: true,
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
                    let mut line = buffer[..pos].to_string();
                    buffer = buffer[pos + 1..].to_string();
                    if line.ends_with('\r') {
                        line.pop();
                    }

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
            }
        };

        let stream: AiStream = Box::pin(stream);
        Ok(stream)
    }

    async fn list_models(&self, cancel: CancellationToken) -> Result<Vec<String>, AiError> {
        let url = self.endpoint("/models")?;
        let fut = async {
            let response = self
                .client
                .get(url)
                .timeout(self.timeout)
                .send()
                .await?
                .error_for_status()?;
            let parsed: OpenAiModelsResponse = response.json().await?;
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
