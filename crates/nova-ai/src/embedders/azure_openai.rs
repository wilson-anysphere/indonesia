use crate::semantic_search::Embedder;
use crate::http::map_reqwest_error;
use crate::AiError;
use reqwest::header::{HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use url::Url;

/// Provider-backed embedder for Azure OpenAI embeddings.
///
/// This is synchronous (uses `reqwest::blocking`) so it can be used from
/// [`crate::EmbeddingSemanticSearch`].
#[derive(Clone)]
pub struct AzureOpenAiEmbedder {
    endpoint: Url,
    deployment: String,
    api_version: String,
    timeout: Duration,
    batch_size: usize,
    client: reqwest::blocking::Client,
}

impl AzureOpenAiEmbedder {
    pub fn new(
        endpoint: Url,
        api_key: String,
        deployment: String,
        api_version: String,
        timeout: Duration,
        batch_size: usize,
    ) -> Result<Self, AiError> {
        let mut headers = HeaderMap::new();
        headers.insert(
            "api-key",
            HeaderValue::from_str(&api_key)
                .map_err(|e| AiError::InvalidConfig(format!("invalid azure api_key: {e}")))?,
        );

        let client = reqwest::blocking::Client::builder()
            .default_headers(headers)
            .timeout(timeout)
            .build()?;

        Ok(Self {
            endpoint,
            deployment,
            api_version,
            timeout,
            batch_size: batch_size.max(1),
            client,
        })
    }

    fn embeddings_url(&self) -> Result<Url, AiError> {
        let mut url = self
            .endpoint
            .join(&format!(
                "openai/deployments/{}/embeddings",
                self.deployment
            ))
            .map_err(|e| AiError::InvalidConfig(e.to_string()))?;
        url.query_pairs_mut()
            .append_pair("api-version", &self.api_version);
        Ok(url)
    }

    fn embed_chunk(&self, input: &[String]) -> Result<Vec<Vec<f32>>, AiError> {
        if input.is_empty() {
            return Ok(Vec::new());
        }

        let url = self.embeddings_url()?;
        let body = AzureEmbeddingsRequest { input };

        let response = self
            .client
            .post(url)
            .json(&body)
            .timeout(self.timeout)
            .send()
            .map_err(map_reqwest_error)?
            .error_for_status()
            .map_err(map_reqwest_error)?;

        let parsed: OpenAiEmbeddingsResponse = response.json().map_err(map_reqwest_error)?;
        parse_openai_embeddings(parsed, input.len())
    }
}

impl Embedder for AzureOpenAiEmbedder {
    fn embed(&self, text: &str) -> Result<Vec<f32>, AiError> {
        let mut out = self.embed_batch(&[text.to_string()])?;
        out.pop()
            .ok_or_else(|| AiError::UnexpectedResponse("missing embedding".into()))
    }

    fn embed_batch(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>, AiError> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }

        let batch_size = self.batch_size.max(1);
        let mut out = Vec::with_capacity(inputs.len());
        for chunk in inputs.chunks(batch_size) {
            out.extend(self.embed_chunk(chunk)?);
        }
        Ok(out)
    }
}

#[derive(Debug, Serialize)]
struct AzureEmbeddingsRequest<'a> {
    input: &'a [String],
}

#[derive(Debug, Deserialize)]
struct OpenAiEmbeddingsResponse {
    data: Vec<OpenAiEmbeddingObject>,
}

#[derive(Debug, Deserialize)]
struct OpenAiEmbeddingObject {
    embedding: Vec<f32>,
    index: usize,
}

fn parse_openai_embeddings(
    response: OpenAiEmbeddingsResponse,
    expected: usize,
) -> Result<Vec<Vec<f32>>, AiError> {
    let mut out = vec![None::<Vec<f32>>; expected];
    for item in response.data {
        if item.index >= expected {
            return Err(AiError::UnexpectedResponse(format!(
                "embeddings index {} out of range (expected < {expected})",
                item.index
            )));
        }
        if out[item.index].is_some() {
            return Err(AiError::UnexpectedResponse(format!(
                "duplicate embeddings index {}",
                item.index
            )));
        }
        out[item.index] = Some(item.embedding);
    }

    out.into_iter()
        .enumerate()
        .map(|(idx, item)| {
            item.filter(|v| !v.is_empty()).ok_or_else(|| {
                AiError::UnexpectedResponse(format!("missing embeddings data for index {idx}"))
            })
        })
        .collect()
}
