use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::{AiError, Embedder as _};
use nova_config::{AiConfig, AiEmbeddingsBackend};

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
            tracing::warn!(
                target = "nova.ai",
                "ai.embeddings.backend=local is not implemented; falling back to hash embeddings"
            );
            Ok(Box::new(LocalEmbeddingsClient::default()))
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
