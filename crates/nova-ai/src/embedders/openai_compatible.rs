use crate::semantic_search::Embedder;
use crate::http::map_reqwest_error;
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
    batch_size: usize,
    client: reqwest::blocking::Client,
}

impl OpenAiCompatibleEmbedder {
    pub fn new(
        base_url: Url,
        model: impl Into<String>,
        timeout: Duration,
        api_key: Option<String>,
        batch_size: usize,
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
            batch_size: batch_size.max(1),
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

    fn embed_request(&self, input: &[String]) -> Result<Vec<Vec<f32>>, AiError> {
        if input.is_empty() {
            return Ok(Vec::new());
        }

        let url = self.endpoint("/embeddings")?;
        let body = OpenAiEmbeddingBatchRequest {
            model: &self.model,
            input,
        };

        let response = self
            .authorize(self.client.post(url))
            .json(&body)
            // Redundant with the client builder timeout, but keep it explicit in case
            // reqwest semantics change.
            .timeout(self.timeout)
            .send()
            .map_err(map_reqwest_error)?
            .error_for_status()
            .map_err(map_reqwest_error)?;

        let bytes = response.bytes().map_err(map_reqwest_error)?;
        let parsed: OpenAiEmbeddingResponse = serde_json::from_slice(&bytes)?;
        parse_openai_embeddings(parsed, input.len())
    }
}

impl Embedder for OpenAiCompatibleEmbedder {
    fn embed(&self, text: &str) -> Result<Vec<f32>, AiError> {
        let input = [text.to_string()];
        let mut embeddings = self.embed_request(&input)?;
        embeddings
            .pop()
            .filter(|embedding| !embedding.is_empty())
            .ok_or_else(|| AiError::UnexpectedResponse("missing data[0].embedding".into()))
    }

    fn embed_batch(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>, AiError> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }

        let batch_size = self.batch_size.max(1);
        let mut out = Vec::with_capacity(inputs.len());
        for chunk in inputs.chunks(batch_size) {
            out.extend(self.embed_request(chunk)?);
        }
        Ok(out)
    }
}

#[derive(Debug, Serialize)]
struct OpenAiEmbeddingBatchRequest<'a> {
    model: &'a str,
    input: &'a [String],
}

#[derive(Debug, Deserialize)]
struct OpenAiEmbeddingResponse {
    data: Vec<OpenAiEmbeddingData>,
}

#[derive(Debug, Deserialize)]
struct OpenAiEmbeddingData {
    embedding: Vec<f32>,
    #[serde(default)]
    index: Option<usize>,
}

fn parse_openai_embeddings(
    response: OpenAiEmbeddingResponse,
    expected: usize,
) -> Result<Vec<Vec<f32>>, AiError> {
    let mut out = vec![None::<Vec<f32>>; expected];

    for (pos, item) in response.data.into_iter().enumerate() {
        let idx = item.index.unwrap_or(pos);
        if idx >= expected {
            return Err(AiError::UnexpectedResponse(format!(
                "embeddings index {} out of range (expected < {expected})",
                idx
            )));
        }
        if out[idx].is_some() {
            return Err(AiError::UnexpectedResponse(format!(
                "duplicate embeddings index {}",
                idx
            )));
        }
        out[idx] = Some(item.embedding);
    }

    let mut dims: Option<usize> = None;
    let mut embeddings = Vec::with_capacity(expected);

    for (idx, item) in out.into_iter().enumerate() {
        let embedding = item.filter(|v| !v.is_empty()).ok_or_else(|| {
            AiError::UnexpectedResponse(format!("missing embeddings data for index {idx}"))
        })?;

        match dims {
            None => dims = Some(embedding.len()),
            Some(expected_dims) if embedding.len() != expected_dims => {
                return Err(AiError::UnexpectedResponse(format!(
                    "inconsistent embedding dimensions: expected {expected_dims}, got {} for index {idx}",
                    embedding.len()
                )));
            }
            _ => {}
        }

        embeddings.push(embedding);
    }

    Ok(embeddings)
}
