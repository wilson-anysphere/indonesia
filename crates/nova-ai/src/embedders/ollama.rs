use crate::semantic_search::Embedder;
use crate::AiError;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use url::Url;

/// Provider-backed embedder for the Ollama `/api/embed` endpoint.
///
/// This is synchronous (uses `reqwest::blocking`) so it can be used from
/// [`crate::EmbeddingSemanticSearch`].
#[derive(Clone)]
pub struct OllamaEmbedder {
    base_url: Url,
    model: String,
    timeout: Duration,
    batch_size: usize,
    client: reqwest::blocking::Client,
}

impl OllamaEmbedder {
    pub fn new(
        base_url: Url,
        model: impl Into<String>,
        timeout: Duration,
        batch_size: usize,
    ) -> Result<Self, AiError> {
        let client = reqwest::blocking::Client::builder()
            .timeout(timeout)
            .build()?;
        Ok(Self {
            base_url,
            model: model.into(),
            timeout,
            batch_size: batch_size.max(1),
            client,
        })
    }

    fn endpoint(&self, path: &str) -> Result<Url, AiError> {
        let base_str = self.base_url.as_str().trim_end_matches('/').to_string();
        let base = Url::parse(&format!("{base_str}/"))?;
        Ok(base.join(path.trim_start_matches('/'))?)
    }

    fn embed_chunk(&self, input: &[String]) -> Result<Vec<Vec<f32>>, AiError> {
        if input.is_empty() {
            return Ok(Vec::new());
        }

        let url = self.endpoint("/api/embed")?;
        let body = OllamaEmbedRequest {
            model: &self.model,
            input,
        };

        let response = self
            .client
            .post(url)
            .json(&body)
            .timeout(self.timeout)
            .send()?
            .error_for_status()?;

        let parsed: OllamaEmbedResponse = response.json()?;
        if let Some(embeddings) = parsed.embeddings {
            if embeddings.len() != input.len() {
                return Err(AiError::UnexpectedResponse(format!(
                    "expected {} embeddings, got {}",
                    input.len(),
                    embeddings.len()
                )));
            }
            return Ok(embeddings);
        }

        if let Some(embedding) = parsed.embedding {
            if input.len() != 1 {
                return Err(AiError::UnexpectedResponse(
                    "ollama returned single embedding for batch request".into(),
                ));
            }
            return Ok(vec![embedding]);
        }

        Err(AiError::UnexpectedResponse(
            "missing embeddings in response".into(),
        ))
    }
}

impl Embedder for OllamaEmbedder {
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
struct OllamaEmbedRequest<'a> {
    model: &'a str,
    input: &'a [String],
}

#[derive(Debug, Deserialize)]
struct OllamaEmbedResponse {
    #[serde(default)]
    embeddings: Option<Vec<Vec<f32>>>,
    #[serde(default)]
    embedding: Option<Vec<f32>>,
}

