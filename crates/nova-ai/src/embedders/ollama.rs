use crate::audit;
use crate::semantic_search::Embedder;
use crate::http::map_reqwest_error;
use crate::AiError;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;
use url::Url;

const OLLAMA_EMBED_ENDPOINT_UNKNOWN: u8 = 0;
const OLLAMA_EMBED_ENDPOINT_SUPPORTED: u8 = 1;
const OLLAMA_EMBED_ENDPOINT_UNSUPPORTED: u8 = 2;

/// Provider-backed embedder for Ollama embeddings.
///
/// Supports both:
/// - `/api/embed` (preferred, batch)
/// - `/api/embeddings` (legacy, one input per request)
///
/// This is synchronous (uses `reqwest::blocking`) so it can be used from
/// [`crate::EmbeddingSemanticSearch`].
#[derive(Clone)]
pub struct OllamaEmbedder {
    base_url: Url,
    model: String,
    timeout: Duration,
    batch_size: usize,
    embed_endpoint: Arc<AtomicU8>,
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
            embed_endpoint: Arc::new(AtomicU8::new(OLLAMA_EMBED_ENDPOINT_UNKNOWN)),
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

    fn embed_via_embed_endpoint(&self, input: &[String]) -> Result<Option<Vec<Vec<f32>>>, AiError> {
        if input.is_empty() {
            return Ok(Some(Vec::new()));
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
            .send()
            .map_err(map_reqwest_error)?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }

        let response = response
            .error_for_status()
            .map_err(map_reqwest_error)?;

        let parsed: OllamaEmbedResponse = response.json().map_err(map_reqwest_error)?;
        if let Some(embeddings) = parsed.embeddings {
            if embeddings.len() != input.len() {
                return Err(AiError::UnexpectedResponse(format!(
                    "expected {} embeddings, got {}",
                    input.len(),
                    embeddings.len()
                )));
            }
            if embeddings.iter().any(|embedding| embedding.is_empty()) {
                return Err(AiError::UnexpectedResponse(
                    "ollama returned empty embedding vector".into(),
                ));
            }
            return Ok(Some(embeddings));
        }

        if let Some(embedding) = parsed.embedding {
            if input.len() != 1 {
                return Err(AiError::UnexpectedResponse(
                    "ollama returned single embedding for batch request".into(),
                ));
            }
            if embedding.is_empty() {
                return Err(AiError::UnexpectedResponse(
                    "ollama returned empty embedding vector".into(),
                ));
            }
            return Ok(Some(vec![embedding]));
        }

        Err(AiError::UnexpectedResponse(
            "missing embeddings in response".into(),
        ))
    }

    fn embed_via_legacy_endpoint(&self, input: &[String]) -> Result<Vec<Vec<f32>>, AiError> {
        if input.is_empty() {
            return Ok(Vec::new());
        }

        let url = self.endpoint("/api/embeddings")?;
        let mut out = Vec::with_capacity(input.len());

        for prompt in input {
            let body = OllamaLegacyEmbedRequest {
                model: &self.model,
                prompt,
            };

            let response = self
                .client
                .post(url.clone())
                .json(&body)
                .timeout(self.timeout)
                .send()
                .map_err(map_reqwest_error)?
                .error_for_status()
                .map_err(map_reqwest_error)?;

            let parsed: OllamaLegacyEmbedResponse = response.json().map_err(map_reqwest_error)?;
            if parsed.embedding.is_empty() {
                return Err(AiError::UnexpectedResponse(
                    "missing `embedding` in Ollama /api/embeddings response".into(),
                ));
            }
            out.push(parsed.embedding);
        }

        Ok(out)
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

        let mut use_embed_endpoint = true;
        let batch_size = self.batch_size.max(1);
        let mut out = Vec::with_capacity(inputs.len());

        for chunk in inputs.chunks(batch_size) {
            let mode = self.embed_endpoint.load(Ordering::Acquire);
            if use_embed_endpoint && mode != OLLAMA_EMBED_ENDPOINT_UNSUPPORTED {
                match self.embed_via_embed_endpoint(chunk) {
                    Ok(Some(embeddings)) => {
                        self.embed_endpoint
                            .store(OLLAMA_EMBED_ENDPOINT_SUPPORTED, Ordering::Release);
                        out.extend(embeddings);
                        continue;
                    }
                    Ok(None) => {
                        self.embed_endpoint
                            .store(OLLAMA_EMBED_ENDPOINT_UNSUPPORTED, Ordering::Release);
                    }
                    Err(err) => {
                        let sanitized_error = audit::sanitize_error_for_tracing(&err.to_string());
                        tracing::warn!(
                            target = "nova.ai",
                            err = %sanitized_error,
                            "Ollama /api/embed failed; falling back to /api/embeddings"
                        );
                        use_embed_endpoint = false;
                    }
                }
            }

            out.extend(self.embed_via_legacy_endpoint(chunk)?);
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

#[derive(Debug, Serialize)]
struct OllamaLegacyEmbedRequest<'a> {
    model: &'a str,
    prompt: &'a str,
}

#[derive(Debug, Deserialize)]
struct OllamaLegacyEmbedResponse {
    #[serde(default)]
    embedding: Vec<f32>,
}
