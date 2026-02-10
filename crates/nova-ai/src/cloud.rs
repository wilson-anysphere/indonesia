use crate::providers::LlmProvider;
use crate::types::{AiStream, ChatMessage, ChatRequest, ChatRole};
use crate::AiError;
use async_stream::try_stream;
use async_trait::async_trait;
use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use url::Url;

/// Anthropic Messages API provider.
#[derive(Clone)]
pub(crate) struct AnthropicProvider {
    base_url: Url,
    model: String,
    timeout: Duration,
    client: reqwest::Client,
}

impl AnthropicProvider {
    pub(crate) fn new(
        base_url: Url,
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
        let base_str = self.base_url.as_str().trim_end_matches('/').to_string();
        let base = Url::parse(&format!("{base_str}/"))?;
        let base_path = base.path().trim_end_matches('/');

        if base_path.ends_with("/v1") {
            Ok(base.join(path.trim_start_matches('/'))?)
        } else {
            Ok(base.join(&format!("v1/{}", path.trim_start_matches('/')))?)
        }
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    async fn chat(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<String, AiError> {
        let url = self.endpoint("/messages")?;

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
        let url = self.endpoint("/messages")?;

        let (system, messages) = anthropic_messages(&request.messages);

        let mut body = serde_json::Map::new();
        body.insert("model".to_string(), json!(self.model));
        body.insert(
            "max_tokens".to_string(),
            json!(request.max_tokens.unwrap_or(1024)),
        );
        body.insert("messages".to_string(), json!(messages));
        body.insert("stream".to_string(), json!(true));
        if !system.trim().is_empty() {
            body.insert("system".to_string(), json!(system));
        }
        if let Some(temp) = request.temperature {
            body.insert("temperature".to_string(), json!(temp));
        }

        let request_builder = self
            .client
            .post(url)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(&body);

        let response = tokio::select! {
            _ = cancel.cancelled() => return Err(AiError::Cancelled),
            resp = tokio::time::timeout(self.timeout, request_builder.send()) => {
                match resp {
                    Ok(res) => res?,
                    Err(_) => return Err(AiError::Timeout),
                }
            }
        }
        .error_for_status()?;

        let mut bytes_stream = response.bytes_stream();
        let timeout = self.timeout;

        let stream = try_stream! {
            let mut decoder = SseDecoder::new();
            let mut events = Vec::<SseEvent>::new();

            'outer: loop {
                let next = tokio::select! {
                    _ = cancel.cancelled() => Err(AiError::Cancelled),
                    chunk = tokio::time::timeout(timeout, bytes_stream.next()) => match chunk {
                        Ok(item) => Ok(item),
                        Err(_) => Err(AiError::Timeout),
                    },
                }?;

                let Some(chunk) = next else { break };
                let chunk = chunk.map_err(AiError::Http)?;
                decoder.push(&chunk, &mut events)?;

                for event in events.drain(..) {
                    if event.event.as_deref() == Some("message_stop") {
                        break 'outer;
                    }
                    if event.data.is_empty() {
                        continue;
                    }
                    let parsed: AnthropicStreamEvent = serde_json::from_slice(&event.data)?;
                    match parsed.kind.as_str() {
                        "content_block_delta" => {
                            if let Some(text) = parsed.delta.and_then(|d| d.text) {
                                if !text.is_empty() {
                                    yield text;
                                }
                            }
                        }
                        "message_stop" => break 'outer,
                        _ => {}
                    }
                }
            }

            decoder.finish(&mut events)?;
            for event in events {
                if event.data.is_empty() {
                    continue;
                }
                let parsed: AnthropicStreamEvent = serde_json::from_slice(&event.data)?;
                match parsed.kind.as_str() {
                    "content_block_delta" => {
                        if let Some(text) = parsed.delta.and_then(|d| d.text) {
                            if !text.is_empty() {
                                yield text;
                            }
                        }
                    }
                    _ => {}
                }
            }
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
    base_url: Url,
    api_key: String,
    model: String,
    timeout: Duration,
    client: reqwest::Client,
}

impl GeminiProvider {
    pub(crate) fn new(
        base_url: Url,
        api_key: String,
        model: impl Into<String>,
        timeout: Duration,
    ) -> Result<Self, AiError> {
        let client = reqwest::Client::builder().build()?;
        Ok(Self {
            base_url,
            api_key,
            model: model.into(),
            timeout,
            client,
        })
    }

    fn endpoint(&self, path: &str) -> Result<Url, AiError> {
        // Accept both:
        // - http://localhost:8000  (we will append /v1beta/...)
        // - http://localhost:8000/v1beta  (we will append /...)
        let base_str = self.base_url.as_str().trim_end_matches('/').to_string();
        let base = Url::parse(&format!("{base_str}/"))?;
        let base_path = base.path().trim_end_matches('/');

        if base_path.ends_with("/v1beta") {
            Ok(base.join(path.trim_start_matches('/'))?)
        } else {
            Ok(base.join(&format!("v1beta/{}", path.trim_start_matches('/')))?)
        }
    }
}

#[async_trait]
impl LlmProvider for GeminiProvider {
    async fn chat(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<String, AiError> {
        let mut url = self.endpoint(&format!(
            "/models/{}:generateContent",
            self.model
        ))?;
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
        let mut url = self.endpoint(&format!(
            "/models/{}:streamGenerateContent",
            self.model
        ))?;
        url.query_pairs_mut()
            .append_pair("key", &self.api_key)
            .append_pair("alt", "sse");

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

        let request_builder = self
            .client
            .post(url)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(&body);

        let response = tokio::select! {
            _ = cancel.cancelled() => return Err(AiError::Cancelled),
            resp = tokio::time::timeout(self.timeout, request_builder.send()) => {
                match resp {
                    Ok(res) => res?,
                    Err(_) => return Err(AiError::Timeout),
                }
            }
        }
        .error_for_status()?;

        let is_sse = content_type_is_event_stream(response.headers());
        let mut bytes_stream = response.bytes_stream();
        let timeout = self.timeout;

        let stream = try_stream! {
            // Gemini streaming responses can be either SSE (`data: {...}` frames) or newline-delimited
            // JSON. We detect based on `Content-Type`, but also accept `data:` frames even when
            // content-type is misconfigured.
            let mut decoder = SseDecoder::new();
            let mut events = Vec::<SseEvent>::new();
            let mut line_buf = LineDecoder::new();
            let mut last_text = String::new();

            loop {
                let next = tokio::select! {
                    _ = cancel.cancelled() => Err(AiError::Cancelled),
                    chunk = tokio::time::timeout(timeout, bytes_stream.next()) => match chunk {
                        Ok(item) => Ok(item),
                        Err(_) => Err(AiError::Timeout),
                    },
                }?;

                let Some(chunk) = next else { break };
                let chunk = chunk.map_err(AiError::Http)?;

                if is_sse {
                    decoder.push(&chunk, &mut events)?;
                    for event in events.drain(..) {
                        if event.data == b"[DONE]" {
                            return;
                        }
                        if event.data.is_empty() {
                            continue;
                        }
                        let parsed: GeminiStreamResponse = serde_json::from_slice(&event.data)?;
                        if let Some(text) = parsed
                            .candidates
                            .into_iter()
                            .next()
                            .and_then(|c| c.content)
                            .and_then(|c| c.parts.into_iter().next())
                            .map(|p| p.text)
                        {
                            let delta = gemini_delta(&mut last_text, &text);
                            if !delta.is_empty() {
                                yield delta;
                            }
                        }
                    }
                    continue;
                }

                // NDJSON or misconfigured content-type. Parse line-by-line, and accept SSE `data:`
                // frames if present.
                line_buf.push(&chunk);
                while let Some(line) = line_buf.next_line()? {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }

                    if let Some(data) = line.strip_prefix("data:") {
                        let data = data.trim();
                        if data == "[DONE]" {
                            return;
                        }
                        let parsed: GeminiStreamResponse = serde_json::from_str(data)?;
                        if let Some(text) = parsed
                            .candidates
                            .into_iter()
                            .next()
                            .and_then(|c| c.content)
                            .and_then(|c| c.parts.into_iter().next())
                            .map(|p| p.text)
                        {
                            let delta = gemini_delta(&mut last_text, &text);
                            if !delta.is_empty() {
                                yield delta;
                            }
                        }
                        continue;
                    }

                    let parsed: GeminiStreamResponse = serde_json::from_str(line)?;
                    if let Some(text) = parsed
                        .candidates
                        .into_iter()
                        .next()
                        .and_then(|c| c.content)
                        .and_then(|c| c.parts.into_iter().next())
                        .map(|p| p.text)
                    {
                        let delta = gemini_delta(&mut last_text, &text);
                        if !delta.is_empty() {
                            yield delta;
                        }
                    }
                }
            }

            if is_sse {
                decoder.finish(&mut events)?;
                for event in events.drain(..) {
                    if event.data == b"[DONE]" {
                        return;
                    }
                    if event.data.is_empty() {
                        continue;
                    }
                    let parsed: GeminiStreamResponse = serde_json::from_slice(&event.data)?;
                    if let Some(text) = parsed
                        .candidates
                        .into_iter()
                        .next()
                        .and_then(|c| c.content)
                        .and_then(|c| c.parts.into_iter().next())
                        .map(|p| p.text)
                    {
                        let delta = gemini_delta(&mut last_text, &text);
                        if !delta.is_empty() {
                            yield delta;
                        }
                    }
                }
                return;
            }

            // Flush any pending buffered lines if the server closed the connection without a final
            // newline.
            for line in line_buf.finish()? {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if let Some(data) = line.strip_prefix("data:") {
                    let data = data.trim();
                    if data == "[DONE]" {
                        return;
                    }
                    let parsed: GeminiStreamResponse = serde_json::from_str(data)?;
                    if let Some(text) = parsed
                        .candidates
                        .into_iter()
                        .next()
                        .and_then(|c| c.content)
                        .and_then(|c| c.parts.into_iter().next())
                        .map(|p| p.text)
                    {
                        let delta = gemini_delta(&mut last_text, &text);
                        if !delta.is_empty() {
                            yield delta;
                        }
                    }
                    continue;
                }
                let parsed: GeminiStreamResponse = serde_json::from_str(line)?;
                if let Some(text) = parsed
                    .candidates
                    .into_iter()
                    .next()
                    .and_then(|c| c.content)
                    .and_then(|c| c.parts.into_iter().next())
                    .map(|p| p.text)
                {
                    let delta = gemini_delta(&mut last_text, &text);
                    if !delta.is_empty() {
                        yield delta;
                    }
                }
            }
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
    base_url: Url,
    deployment: String,
    api_version: String,
    timeout: Duration,
    client: reqwest::Client,
}

impl AzureOpenAiProvider {
    pub(crate) fn new(
        base_url: Url,
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
            base_url,
            deployment,
            api_version,
            timeout,
            client,
        })
    }

    fn endpoint(&self, path: &str) -> Result<Url, AiError> {
        // Accept both:
        // - http://localhost:8000  (we will append /openai/...)
        // - http://localhost:8000/openai  (we will append /...)
        let base_str = self.base_url.as_str().trim_end_matches('/').to_string();
        let base = Url::parse(&format!("{base_str}/"))?;
        let base_path = base.path().trim_end_matches('/');

        if base_path.ends_with("/openai") {
            Ok(base.join(path.trim_start_matches('/'))?)
        } else {
            Ok(base.join(&format!("openai/{}", path.trim_start_matches('/')))?)
        }
    }
}

#[async_trait]
impl LlmProvider for AzureOpenAiProvider {
    async fn chat(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<String, AiError> {
        let mut url = self.endpoint(&format!(
            "/deployments/{}/chat/completions",
            self.deployment
        ))?;
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
        let mut url = self.endpoint(&format!(
            "/deployments/{}/chat/completions",
            self.deployment
        ))?;
        url.query_pairs_mut()
            .append_pair("api-version", &self.api_version);

        let mut body = serde_json::Map::new();
        body.insert("messages".to_string(), json!(request.messages));
        body.insert(
            "max_tokens".to_string(),
            json!(request.max_tokens.unwrap_or(1024)),
        );
        body.insert("stream".to_string(), json!(true));
        if let Some(temp) = request.temperature {
            body.insert("temperature".to_string(), json!(temp));
        }

        let request_builder = self
            .client
            .post(url)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(&body);

        let response = tokio::select! {
            _ = cancel.cancelled() => return Err(AiError::Cancelled),
            resp = tokio::time::timeout(self.timeout, request_builder.send()) => {
                match resp {
                    Ok(res) => res?,
                    Err(_) => return Err(AiError::Timeout),
                }
            }
        }
        .error_for_status()?;

        let mut bytes_stream = response.bytes_stream();
        let timeout = self.timeout;

        let stream = try_stream! {
            let mut decoder = SseDecoder::new();
            let mut events = Vec::<SseEvent>::new();

            'outer: loop {
                let next = tokio::select! {
                    _ = cancel.cancelled() => Err(AiError::Cancelled),
                    chunk = tokio::time::timeout(timeout, bytes_stream.next()) => match chunk {
                        Ok(item) => Ok(item),
                        Err(_) => Err(AiError::Timeout),
                    },
                }?;

                let Some(chunk) = next else { break };
                let chunk = chunk.map_err(AiError::Http)?;
                decoder.push(&chunk, &mut events)?;

                for event in events.drain(..) {
                    if event.data == b"[DONE]" {
                        break 'outer;
                    }
                    if event.data.is_empty() {
                        continue;
                    }

                    let parsed: OpenAiChatCompletionStreamResponse = serde_json::from_slice(&event.data)?;
                    for choice in parsed.choices {
                        if let Some(content) = choice.delta.content {
                            if !content.is_empty() {
                                yield content;
                            }
                        }
                    }
                }
            }

            decoder.finish(&mut events)?;
            for event in events {
                if event.data == b"[DONE]" {
                    break;
                }
                if event.data.is_empty() {
                    continue;
                }
                let parsed: OpenAiChatCompletionStreamResponse = serde_json::from_slice(&event.data)?;
                for choice in parsed.choices {
                    if let Some(content) = choice.delta.content {
                        if !content.is_empty() {
                            yield content;
                        }
                    }
                }
            }
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
        let prompt = messages_to_prompt(&request.messages);

        let mut body = serde_json::Map::new();
        body.insert("model".to_string(), json!(self.model));
        body.insert("prompt".to_string(), json!(prompt));
        body.insert(
            "max_tokens".to_string(),
            json!(request.max_tokens.unwrap_or(1024)),
        );
        body.insert("stream".to_string(), json!(true));
        if let Some(temp) = request.temperature {
            body.insert("temperature".to_string(), json!(temp));
        }

        let request_builder = self
            .client
            .post(self.endpoint.clone())
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(&body);

        let response = tokio::select! {
            _ = cancel.cancelled() => return Err(AiError::Cancelled),
            resp = tokio::time::timeout(self.timeout, request_builder.send()) => {
                match resp {
                    Ok(res) => res?,
                    Err(_) => return Err(AiError::Timeout),
                }
            }
        }
        .error_for_status()?;

        // If the endpoint doesn't support SSE, fall back to a standard JSON response.
        if !content_type_is_event_stream(response.headers()) {
            let bytes = tokio::select! {
                _ = cancel.cancelled() => return Err(AiError::Cancelled),
                res = tokio::time::timeout(self.timeout, response.bytes()) => match res {
                    Ok(out) => out.map_err(AiError::Http),
                    Err(_) => Err(AiError::Timeout),
                },
            }?;
            let out = parse_http_completion(&bytes)?;
            let stream = try_stream! {
                if !out.is_empty() {
                    yield out;
                }
            };
            return Ok(Box::pin(stream));
        }

        let mut bytes_stream = response.bytes_stream();
        let timeout = self.timeout;

        let stream = try_stream! {
            let mut decoder = SseDecoder::new();
            let mut events = Vec::<SseEvent>::new();

            'outer: loop {
                let next = tokio::select! {
                    _ = cancel.cancelled() => Err(AiError::Cancelled),
                    chunk = tokio::time::timeout(timeout, bytes_stream.next()) => match chunk {
                        Ok(item) => Ok(item),
                        Err(_) => Err(AiError::Timeout),
                    },
                }?;

                let Some(chunk) = next else { break };
                let chunk = chunk.map_err(AiError::Http)?;
                decoder.push(&chunk, &mut events)?;

                for event in events.drain(..) {
                    if event.data == b"[DONE]" {
                        break 'outer;
                    }
                    if event.data.is_empty() {
                        continue;
                    }
                    let parsed: HttpStreamResponse = serde_json::from_slice(&event.data)?;
                    let completion = parsed.completion.ok_or_else(|| {
                        AiError::UnexpectedResponse("missing completion field".into())
                    })?;
                    if completion.is_empty() {
                        continue;
                    }
                    yield completion;
                }
            }

            decoder.finish(&mut events)?;
            for event in events {
                if event.data == b"[DONE]" {
                    break;
                }
                if event.data.is_empty() {
                    continue;
                }
                let parsed: HttpStreamResponse = serde_json::from_slice(&event.data)?;
                let completion = parsed.completion.ok_or_else(|| {
                    AiError::UnexpectedResponse("missing completion field".into())
                })?;
                if completion.is_empty() {
                    continue;
                }
                yield completion;
            }
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

#[derive(Debug, Deserialize)]
struct AnthropicStreamEvent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    delta: Option<AnthropicStreamDelta>,
}

#[derive(Debug, Deserialize)]
struct AnthropicStreamDelta {
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GeminiStreamResponse {
    #[serde(default)]
    candidates: Vec<GeminiStreamCandidate>,
}

#[derive(Debug, Deserialize)]
struct GeminiStreamCandidate {
    #[serde(default)]
    content: Option<GeminiStreamContent>,
}

#[derive(Debug, Deserialize)]
struct GeminiStreamContent {
    #[serde(default)]
    parts: Vec<GeminiStreamPart>,
}

#[derive(Debug, Deserialize)]
struct GeminiStreamPart {
    #[serde(default)]
    text: String,
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
struct HttpStreamResponse {
    completion: Option<String>,
}

fn content_type_is_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|ct| ct.split(';').next())
        .map(str::trim)
        .is_some_and(|ct| ct.eq_ignore_ascii_case("text/event-stream"))
}

fn gemini_delta(last_text: &mut String, chunk_text: &str) -> String {
    if chunk_text.is_empty() {
        return String::new();
    }

    if chunk_text.starts_with(last_text.as_str()) {
        let delta = chunk_text[last_text.len()..].to_string();
        *last_text = chunk_text.to_string();
        delta
    } else if last_text.ends_with(chunk_text) {
        // Already emitted.
        String::new()
    } else {
        last_text.push_str(chunk_text);
        chunk_text.to_string()
    }
}

#[derive(Debug)]
struct SseEvent {
    #[allow(dead_code)]
    event: Option<String>,
    data: Vec<u8>,
}

#[derive(Default)]
struct SseDecoder {
    buffer: Vec<u8>,
    current_event: Option<String>,
    current_data: Vec<u8>,
    saw_any_field: bool,
}

impl SseDecoder {
    fn new() -> Self {
        Self::default()
    }

    fn push(&mut self, chunk: &[u8], out: &mut Vec<SseEvent>) -> Result<(), AiError> {
        self.buffer.extend_from_slice(chunk);
        while let Some(pos) = self.buffer.iter().position(|&b| b == b'\n') {
            let rest = self.buffer.split_off(pos + 1);
            let mut line = std::mem::take(&mut self.buffer);
            self.buffer = rest;

            // Remove trailing '\n' and optional '\r'.
            if line.last() == Some(&b'\n') {
                line.pop();
            }
            if line.last() == Some(&b'\r') {
                line.pop();
            }

            self.process_line(&line, out)?;
        }
        Ok(())
    }

    fn finish(&mut self, out: &mut Vec<SseEvent>) -> Result<(), AiError> {
        if !self.buffer.is_empty() {
            let mut line = std::mem::take(&mut self.buffer);
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.process_line(&line, out)?;
        }

        if self.saw_any_field {
            out.push(SseEvent {
                event: self.current_event.take(),
                data: std::mem::take(&mut self.current_data),
            });
            self.saw_any_field = false;
        }

        Ok(())
    }

    fn process_line(&mut self, line: &[u8], out: &mut Vec<SseEvent>) -> Result<(), AiError> {
        if line.is_empty() {
            if self.saw_any_field {
                out.push(SseEvent {
                    event: self.current_event.take(),
                    data: std::mem::take(&mut self.current_data),
                });
                self.saw_any_field = false;
            }
            return Ok(());
        }

        if line.starts_with(b":") {
            // Comment/keepalive line.
            return Ok(());
        }

        if let Some(rest) = line.strip_prefix(b"event:") {
            let rest = trim_sse_value(rest);
            let event = std::str::from_utf8(rest).map_err(|e| {
                AiError::UnexpectedResponse(format!("invalid utf-8 in sse event field: {e}"))
            })?;
            self.current_event = Some(event.to_string());
            self.saw_any_field = true;
            return Ok(());
        }

        if let Some(rest) = line.strip_prefix(b"data:") {
            let rest = trim_sse_value(rest);
            if !self.current_data.is_empty() {
                self.current_data.push(b'\n');
            }
            self.current_data.extend_from_slice(rest);
            self.saw_any_field = true;
            return Ok(());
        }

        // Ignore other fields (id:, retry:, etc).
        Ok(())
    }
}

fn trim_sse_value(mut bytes: &[u8]) -> &[u8] {
    while let Some((&b, rest)) = bytes.split_first() {
        if b == b' ' || b == b'\t' {
            bytes = rest;
        } else {
            break;
        }
    }
    while let Some((&b, rest)) = bytes.split_last() {
        if b == b' ' || b == b'\t' {
            bytes = rest;
        } else {
            break;
        }
    }
    bytes
}

#[derive(Default)]
struct LineDecoder {
    buffer: Vec<u8>,
    lines: std::collections::VecDeque<Vec<u8>>,
}

impl LineDecoder {
    fn new() -> Self {
        Self::default()
    }

    fn push(&mut self, chunk: &[u8]) {
        self.buffer.extend_from_slice(chunk);
        while let Some(pos) = self.buffer.iter().position(|&b| b == b'\n') {
            let rest = self.buffer.split_off(pos + 1);
            let mut line = std::mem::take(&mut self.buffer);
            self.buffer = rest;

            if line.last() == Some(&b'\n') {
                line.pop();
            }
            if line.last() == Some(&b'\r') {
                line.pop();
            }

            self.lines.push_back(line);
        }
    }

    fn next_line(&mut self) -> Result<Option<String>, AiError> {
        if let Some(line) = self.lines.pop_front() {
            let s = String::from_utf8(line).map_err(|e| {
                AiError::UnexpectedResponse(format!("invalid utf-8 in streamed response: {e}"))
            })?;
            return Ok(Some(s));
        }
        Ok(None)
    }

    fn finish(&mut self) -> Result<Vec<String>, AiError> {
        let mut out = Vec::new();
        while let Some(line) = self.lines.pop_front() {
            let s = String::from_utf8(line).map_err(|e| {
                AiError::UnexpectedResponse(format!("invalid utf-8 in streamed response: {e}"))
            })?;
            out.push(s);
        }
        if !self.buffer.is_empty() {
            let mut line = std::mem::take(&mut self.buffer);
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let s = String::from_utf8(line).map_err(|e| {
                AiError::UnexpectedResponse(format!("invalid utf-8 in streamed response: {e}"))
            })?;
            out.push(s);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use futures::TryStreamExt;
    use hyper::{
        body::Bytes,
        service::{make_service_fn, service_fn},
        Body, Request, Response, Server, StatusCode,
    };
    use std::convert::Infallible;
    use tokio::sync::oneshot;

    fn spawn_server<F>(handler: F) -> (Url, oneshot::Sender<()>)
    where
        F: Fn(Request<Body>) -> Response<Body> + Send + Sync + 'static,
    {
        let handler = std::sync::Arc::new(handler);
        let make_svc = make_service_fn(move |_| {
            let handler = handler.clone();
            async move {
                Ok::<_, Infallible>(service_fn(move |req| {
                    let handler = handler.clone();
                    async move { Ok::<_, Infallible>((handler)(req)) }
                }))
            }
        });

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test server");
        listener
            .set_nonblocking(true)
            .expect("set_nonblocking");
        let addr = listener.local_addr().expect("local addr");
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        let server = Server::from_tcp(listener)
            .expect("server from listener")
            .serve(make_svc)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            });

        tokio::spawn(async move {
            let _ = server.await;
        });

        let url = Url::parse(&format!("http://{addr}/")).expect("valid url");
        (url, shutdown_tx)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn http_provider_streaming_parses_sse_and_preserves_utf8_across_chunk_boundaries() {
        let (url, shutdown) = spawn_server(|_req| {
            let stream = async_stream::stream! {
                // Split the UTF-8 bytes for 'é' (0xC3 0xA9) across chunks.
                yield Ok::<Bytes, Infallible>(Bytes::from_static(b"data: {\"completion\":\"h\xc3"));
                yield Ok::<Bytes, Infallible>(Bytes::from_static(b"\xa9\"}\n\n"));
                yield Ok::<Bytes, Infallible>(Bytes::from_static(b"data: [DONE]\n\n"));
            };

            let mut resp = Response::new(Body::wrap_stream(stream));
            *resp.status_mut() = StatusCode::OK;
            resp.headers_mut().insert(
                hyper::header::CONTENT_TYPE,
                hyper::header::HeaderValue::from_static("text/event-stream"),
            );
            resp
        });

        let provider = HttpProvider::new(url, None, "test-model", Duration::from_secs(5))
            .expect("provider");
        let stream = provider
            .chat_stream(
                ChatRequest {
                    messages: vec![ChatMessage::user("hello")],
                    max_tokens: None,
                    temperature: None,
                },
                CancellationToken::new(),
            )
            .await
            .expect("stream starts");

        let parts: Vec<String> = stream.try_collect().await.expect("stream ok");
        assert_eq!(parts.concat(), "hé");

        let _ = shutdown.send(());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn http_provider_streaming_supports_idle_timeouts() {
        let (url, shutdown) = spawn_server(|_req| {
            let stream = async_stream::stream! {
                yield Ok::<Bytes, Infallible>(Bytes::from_static(b"data: {\"completion\":\"a\"}\n\n"));
                tokio::time::sleep(Duration::from_millis(200)).await;
                yield Ok::<Bytes, Infallible>(Bytes::from_static(b"data: {\"completion\":\"b\"}\n\n"));
            };

            let mut resp = Response::new(Body::wrap_stream(stream));
            *resp.status_mut() = StatusCode::OK;
            resp.headers_mut().insert(
                hyper::header::CONTENT_TYPE,
                hyper::header::HeaderValue::from_static("text/event-stream"),
            );
            resp
        });

        let provider =
            HttpProvider::new(url, None, "test-model", Duration::from_millis(50)).expect("provider");
        let mut stream = provider
            .chat_stream(
                ChatRequest {
                    messages: vec![ChatMessage::user("hello")],
                    max_tokens: None,
                    temperature: None,
                },
                CancellationToken::new(),
            )
            .await
            .expect("stream starts");

        assert_eq!(stream.next().await.transpose().expect("ok"), Some("a".to_string()));
        let err = stream.next().await.expect("error item").expect_err("timeout");
        assert!(matches!(err, AiError::Timeout));

        let _ = shutdown.send(());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn http_provider_streaming_supports_cancellation_while_waiting_for_next_chunk() {
        let (url, shutdown) = spawn_server(|_req| {
            let stream = async_stream::stream! {
                yield Ok::<Bytes, Infallible>(Bytes::from_static(b"data: {\"completion\":\"a\"}\n\n"));
                tokio::time::sleep(Duration::from_secs(5)).await;
                yield Ok::<Bytes, Infallible>(Bytes::from_static(b"data: {\"completion\":\"b\"}\n\n"));
            };

            let mut resp = Response::new(Body::wrap_stream(stream));
            *resp.status_mut() = StatusCode::OK;
            resp.headers_mut().insert(
                hyper::header::CONTENT_TYPE,
                hyper::header::HeaderValue::from_static("text/event-stream"),
            );
            resp
        });

        let provider = HttpProvider::new(url, None, "test-model", Duration::from_secs(30))
            .expect("provider");
        let cancel = CancellationToken::new();
        let mut stream = provider
            .chat_stream(
                ChatRequest {
                    messages: vec![ChatMessage::user("hello")],
                    max_tokens: None,
                    temperature: None,
                },
                cancel.clone(),
            )
            .await
            .expect("stream starts");

        assert_eq!(stream.next().await.transpose().expect("ok"), Some("a".to_string()));
        cancel.cancel();

        let err = stream.next().await.expect("error item").expect_err("cancelled");
        assert!(matches!(err, AiError::Cancelled));

        let _ = shutdown.send(());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn azure_openai_provider_streaming_parses_openai_sse() {
        let (url, shutdown) = spawn_server(|req| {
            assert_eq!(
                req.uri().path(),
                "/openai/deployments/test-deploy/chat/completions"
            );
            assert_eq!(req.uri().query(), Some("api-version=2024-02-01"));

            let stream = async_stream::stream! {
                yield Ok::<Bytes, Infallible>(Bytes::from_static(b"data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n"));
                yield Ok::<Bytes, Infallible>(Bytes::from_static(b"data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n"));
                yield Ok::<Bytes, Infallible>(Bytes::from_static(b"data: [DONE]\n\n"));
            };

            let mut resp = Response::new(Body::wrap_stream(stream));
            *resp.status_mut() = StatusCode::OK;
            resp.headers_mut().insert(
                hyper::header::CONTENT_TYPE,
                hyper::header::HeaderValue::from_static("text/event-stream"),
            );
            resp
        });

        let provider = AzureOpenAiProvider::new(
            url,
            "dummy-key".to_string(),
            "test-deploy".to_string(),
            "2024-02-01".to_string(),
            Duration::from_secs(5),
        )
        .expect("provider");

        let stream = provider
            .chat_stream(
                ChatRequest {
                    messages: vec![ChatMessage::user("hello")],
                    max_tokens: None,
                    temperature: None,
                },
                CancellationToken::new(),
            )
            .await
            .expect("stream starts");
        let parts: Vec<String> = stream.try_collect().await.expect("stream ok");
        assert_eq!(parts.concat(), "Hello");

        let _ = shutdown.send(());
    }
}
