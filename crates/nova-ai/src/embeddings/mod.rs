use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::{AiError, Embedder as _};
use nova_config::{AiConfig, AiEmbeddingsBackend};
#[cfg(feature = "embeddings-local")]
use std::sync::Arc;

pub mod cache;

use cache::{EmbeddingCacheKey, EmbeddingVectorCache};

pub(crate) mod disk_cache;
mod provider;

/// An embeddings backend which can produce vector embeddings for batches of input strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingInputKind {
    /// Code-like inputs (semantic-search indexed documents).
    Document,
    /// Natural language inputs (best-effort sanitization).
    Query,
}

/// An embeddings backend which can produce vector embeddings for batches of input strings.
#[async_trait]
pub trait EmbeddingsClient: Send + Sync {
    async fn embed(
        &self,
        input: &[String],
        kind: EmbeddingInputKind,
        cancel: CancellationToken,
    ) -> Result<Vec<Vec<f32>>, AiError>;
}

/// Construct an [`EmbeddingsClient`] from runtime config.
pub fn embeddings_client_from_config(config: &AiConfig) -> Result<Box<dyn EmbeddingsClient>, AiError> {
    let max_memory_bytes = (config.embeddings.max_memory_bytes.0).min(usize::MAX as u64) as usize;

    match config.embeddings.backend {
        AiEmbeddingsBackend::Hash => Ok(Box::new(LocalEmbeddingsClient::new(max_memory_bytes))),
        AiEmbeddingsBackend::Provider => {
            provider::provider_embeddings_client_from_config(config, max_memory_bytes)
        }
        AiEmbeddingsBackend::Local => {
            #[cfg(feature = "embeddings-local")]
            let out: Result<Box<dyn EmbeddingsClient>, AiError> =
                match crate::LocalEmbedder::from_config(&config.embeddings) {
                    Ok(embedder) => Ok(Box::new(LocalNeuralEmbeddingsClient::new(
                        Arc::new(embedder),
                        max_memory_bytes,
                        config.embeddings.local_model.trim().to_string(),
                    ))),
                    Err(err) => {
                        let sanitized_error = crate::audit::sanitize_error_for_tracing(&err.to_string());
                        tracing::warn!(
                            target = "nova.ai",
                            err = %sanitized_error,
                            "failed to initialize local embeddings; falling back to hash embeddings"
                        );
                        Ok(Box::new(LocalEmbeddingsClient::new(max_memory_bytes)))
                    }
                };

            #[cfg(not(feature = "embeddings-local"))]
            let out: Result<Box<dyn EmbeddingsClient>, AiError> = {
                tracing::warn!(
                    target = "nova.ai",
                    "ai.embeddings.backend=local but nova-ai was built without the `embeddings-local` feature; falling back to hash embeddings"
                );
                Ok(Box::new(LocalEmbeddingsClient::new(max_memory_bytes)))
            };

            out
        }
    }
}

#[derive(Debug)]
struct LocalEmbeddingsClient {
    embedder: crate::HashEmbedder,
    cache: EmbeddingVectorCache,
    model: String,
}

impl LocalEmbeddingsClient {
    fn new(max_memory_bytes: usize) -> Self {
        let embedder = crate::HashEmbedder::default();
        let model = format!("hash:dims={}", embedder.dims());
        Self {
            embedder,
            cache: EmbeddingVectorCache::new(max_memory_bytes),
            model,
        }
    }
}

#[async_trait]
impl EmbeddingsClient for LocalEmbeddingsClient {
    async fn embed(
        &self,
        input: &[String],
        _kind: EmbeddingInputKind,
        cancel: CancellationToken,
    ) -> Result<Vec<Vec<f32>>, AiError> {
        if input.is_empty() {
            return Ok(Vec::new());
        }
        if cancel.is_cancelled() {
            return Err(AiError::Cancelled);
        }

        let mut out = vec![None::<Vec<f32>>; input.len()];
        let mut miss_indices = Vec::new();
        let mut miss_keys = Vec::new();
        let mut miss_inputs = Vec::new();

        for (idx, text) in input.iter().enumerate() {
            let key = EmbeddingCacheKey::new("hash", &self.model, text);
            if let Some(hit) = self.cache.get(key) {
                out[idx] = Some(hit);
            } else {
                miss_indices.push(idx);
                miss_keys.push(key);
                miss_inputs.push(text.clone());
            }
        }

        if !miss_inputs.is_empty() {
            let embeddings = self.embedder.embed_batch(&miss_inputs)?;
            if embeddings.len() != miss_inputs.len() {
                return Err(AiError::UnexpectedResponse(format!(
                    "embedder returned unexpected batch size: expected {}, got {}",
                    miss_inputs.len(),
                    embeddings.len()
                )));
            }

            for ((orig_idx, key), embedding) in miss_indices
                .into_iter()
                .zip(miss_keys.into_iter())
                .zip(embeddings.into_iter())
            {
                out[orig_idx] = Some(embedding.clone());
                self.cache.insert(key, embedding);
            }
        }

        out.into_iter()
            .enumerate()
            .map(|(idx, item)| {
                item.ok_or_else(|| {
                    AiError::UnexpectedResponse(format!("missing embedding output for index {idx}"))
                })
            })
            .collect()
    }
}

#[cfg(feature = "embeddings-local")]
#[derive(Debug)]
struct LocalNeuralEmbeddingsClient {
    embedder: Arc<crate::LocalEmbedder>,
    cache: EmbeddingVectorCache,
    model: String,
}

#[cfg(feature = "embeddings-local")]
impl LocalNeuralEmbeddingsClient {
    fn new(embedder: Arc<crate::LocalEmbedder>, max_memory_bytes: usize, model: String) -> Self {
        Self {
            embedder,
            cache: EmbeddingVectorCache::new(max_memory_bytes),
            model,
        }
    }
}

#[cfg(feature = "embeddings-local")]
#[async_trait]
impl EmbeddingsClient for LocalNeuralEmbeddingsClient {
    async fn embed(
        &self,
        input: &[String],
        kind: EmbeddingInputKind,
        cancel: CancellationToken,
    ) -> Result<Vec<Vec<f32>>, AiError> {
        if input.is_empty() {
            return Ok(Vec::new());
        }
        if cancel.is_cancelled() {
            return Err(AiError::Cancelled);
        }

        let mut out = vec![None::<Vec<f32>>; input.len()];
        let mut miss_indices = Vec::new();
        let mut miss_keys = Vec::new();
        let mut miss_inputs = Vec::new();

        let embedder_identity = match kind {
            EmbeddingInputKind::Document => "local_document",
            EmbeddingInputKind::Query => "local_query",
        };

        for (idx, text) in input.iter().enumerate() {
            let key = EmbeddingCacheKey::new(embedder_identity, &self.model, text);
            if let Some(hit) = self.cache.get(key) {
                out[idx] = Some(hit);
            } else {
                miss_indices.push(idx);
                miss_keys.push(key);
                miss_inputs.push(text.clone());
            }
        }

        if !miss_inputs.is_empty() {
            let expected = miss_inputs.len();
            let embedder = self.embedder.clone();
            let mut task = tokio::task::spawn_blocking(move || embedder.embed_batch(&miss_inputs));

            let embeddings = tokio::select! {
                _ = cancel.cancelled() => {
                    task.abort();
                    Err(AiError::Cancelled)
                }
                res = &mut task => match res {
                    Ok(inner) => inner,
                    Err(err) => Err(AiError::UnexpectedResponse(format!(
                        "local embedder task failed: {err}"
                    ))),
                },
            }?;

            if cancel.is_cancelled() {
                return Err(AiError::Cancelled);
            }

            if embeddings.len() != expected {
                return Err(AiError::UnexpectedResponse(format!(
                    "embedder returned unexpected batch size: expected {expected}, got {}",
                    embeddings.len()
                )));
            }

            for ((orig_idx, key), embedding) in miss_indices
                .into_iter()
                .zip(miss_keys.into_iter())
                .zip(embeddings.into_iter())
            {
                out[orig_idx] = Some(embedding.clone());
                self.cache.insert(key, embedding);
            }
        }

        out.into_iter()
            .enumerate()
            .map(|(idx, item)| {
                item.ok_or_else(|| {
                    AiError::UnexpectedResponse(format!("missing embedding output for index {idx}"))
                })
            })
            .collect()
    }
}
