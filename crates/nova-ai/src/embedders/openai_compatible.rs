use crate::semantic_search::Embedder;
use crate::AiError;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use url::Url;

/// Provider-backed embedder using the OpenAI embeddings API shape.
///
/// This type is intentionally **synchronous**: it uses `reqwest::blocking` so
/// it can be used from [`crate::EmbeddingSemanticSearch`] (which is sync) from
/// contexts without an existing tokio runtime.
#[derive(Clone)]
pub struct OpenAiCompatibleEmbedder {
    base_url: Url,
    model: String,
    timeout: Duration,
    api_key: Option<String>,
    client: reqwest::blocking::Client,
}

impl OpenAiCompatibleEmbedder {
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

        let client = reqwest::blocking::Client::builder()
            .default_headers(headers)
            .timeout(timeout)
            .build()?;

        Ok(Self {
            base_url,
            model: model.into(),
            timeout,
            api_key,
            client,
        })
    }

    fn authorize(&self, request: reqwest::blocking::RequestBuilder) -> reqwest::blocking::RequestBuilder {
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

    fn embed_once(&self, text: &str) -> Result<Vec<f32>, AiError> {
        let url = self.endpoint("/embeddings")?;
        let body = OpenAiEmbeddingRequest {
            model: &self.model,
            input: text,
        };

        let response = self
            .authorize(self.client.post(url))
            .json(&body)
            // Redundant with the client builder timeout, but keep it explicit in case
            // reqwest semantics change.
            .timeout(self.timeout)
            .send()?
            .error_for_status()?;

        let parsed: OpenAiEmbeddingResponse = response.json()?;
        let embedding = parsed
            .data
            .into_iter()
            .next()
            .map(|item| item.embedding)
            .filter(|embedding| !embedding.is_empty())
            .ok_or_else(|| AiError::UnexpectedResponse("missing data[0].embedding".into()))?;

        Ok(embedding)
    }
}

impl Embedder for OpenAiCompatibleEmbedder {
    fn embed(&self, text: &str) -> Vec<f32> {
        match self.embed_once(text) {
            Ok(vec) => vec,
            Err(err) => {
                tracing::warn!(target = "nova.ai", "embedding request failed: {err}");
                Vec::new()
            }
        }
    }
}

#[derive(Debug, Serialize)]
struct OpenAiEmbeddingRequest<'a> {
    model: &'a str,
    input: &'a str,
}

#[derive(Debug, Deserialize)]
struct OpenAiEmbeddingResponse {
    data: Vec<OpenAiEmbeddingData>,
}

#[derive(Debug, Deserialize)]
struct OpenAiEmbeddingData {
    embedding: Vec<f32>,
}

