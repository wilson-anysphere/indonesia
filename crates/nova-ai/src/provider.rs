use crate::cancel::CancellationToken as SyncCancellationToken;
use futures::future::BoxFuture;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::MultiTokenCompletion;

#[derive(Debug, Error)]
pub enum AiProviderError {
    #[error("AI request cancelled")]
    Cancelled,
    #[error("AI request timed out")]
    Timeout,
    #[error("AI provider error: {0}")]
    Provider(String),
}

pub trait AiProvider: Send + Sync {
    fn complete(
        &self,
        prompt: &str,
        cancel: &SyncCancellationToken,
    ) -> Result<String, AiProviderError>;
}

/// An AI provider capable of producing multi-token completion suggestions.
pub trait MultiTokenCompletionProvider: Send + Sync {
    fn complete_multi_token<'a>(
        &'a self,
        prompt: String,
        max_items: usize,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<MultiTokenCompletion>, AiProviderError>>;
}
