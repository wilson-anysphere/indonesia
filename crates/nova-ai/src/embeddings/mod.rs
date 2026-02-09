use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::{AiError, Embedder as _};
use nova_config::{AiConfig, AiEmbeddingsBackend};
#[cfg(feature = "embeddings-local")]
use std::sync::Arc;

pub(crate) mod disk_cache;
mod provider;

/// An embeddings backend which can produce vector embeddings for batches of input strings.
#[async_trait]
pub trait EmbeddingsClient: Send + Sync {
    async fn embed(
        &self,
        input: &[String],
        cancel: CancellationToken,
    ) -> Result<Vec<Vec<f32>>, AiError>;
}

/// Construct an [`EmbeddingsClient`] from runtime config.
pub fn embeddings_client_from_config(config: &AiConfig) -> Result<Box<dyn EmbeddingsClient>, AiError> {
    match config.embeddings.backend {
        AiEmbeddingsBackend::Hash => Ok(Box::new(LocalEmbeddingsClient::default())),
        AiEmbeddingsBackend::Provider => provider::provider_embeddings_client_from_config(config),
        AiEmbeddingsBackend::Local => {
            #[cfg(feature = "embeddings-local")]
            let out: Result<Box<dyn EmbeddingsClient>, AiError> = {
                let embedder = crate::LocalEmbedder::from_config(&config.embeddings)?;
                Ok(Box::new(LocalNeuralEmbeddingsClient {
                    embedder: Arc::new(embedder),
                }))
            };

            #[cfg(not(feature = "embeddings-local"))]
            let out: Result<Box<dyn EmbeddingsClient>, AiError> = {
                tracing::warn!(
                    target = "nova.ai",
                    "ai.embeddings.backend=local but nova-ai was built without the `embeddings-local` feature; falling back to hash embeddings"
                );
                Ok(Box::new(LocalEmbeddingsClient::default()))
            };

            out
        }
    }
}

#[derive(Debug, Default)]
struct LocalEmbeddingsClient {
    embedder: crate::HashEmbedder,
}

#[async_trait]
impl EmbeddingsClient for LocalEmbeddingsClient {
    async fn embed(
        &self,
        input: &[String],
        _cancel: CancellationToken,
    ) -> Result<Vec<Vec<f32>>, AiError> {
        self.embedder.embed_batch(input)
    }
}

#[cfg(feature = "embeddings-local")]
#[derive(Debug)]
struct LocalNeuralEmbeddingsClient {
    embedder: Arc<crate::LocalEmbedder>,
}

#[cfg(feature = "embeddings-local")]
#[async_trait]
impl EmbeddingsClient for LocalNeuralEmbeddingsClient {
    async fn embed(
        &self,
        input: &[String],
        cancel: CancellationToken,
    ) -> Result<Vec<Vec<f32>>, AiError> {
        if input.is_empty() {
            return Ok(Vec::new());
        }
        if cancel.is_cancelled() {
            return Err(AiError::Cancelled);
        }

        let embedder = self.embedder.clone();
        let owned = input.to_vec();
        let embeddings = tokio::task::spawn_blocking(move || embedder.embed_batch(&owned))
            .await
            .map_err(|err| {
                AiError::UnexpectedResponse(format!("local embedder task failed: {err}"))
            })??;

        if cancel.is_cancelled() {
            return Err(AiError::Cancelled);
        }

        Ok(embeddings)
    }
}
