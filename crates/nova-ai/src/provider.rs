use crate::cancel::CancellationToken;
use futures::future::BoxFuture;
use thiserror::Error;

use crate::MultiTokenCompletion;

#[derive(Debug, Error)]
pub enum AiProviderError {
    #[error("AI request cancelled")]
    Cancelled,
    #[error("AI provider error: {0}")]
    Provider(String),
}

pub trait AiProvider: Send + Sync {
    fn complete(&self, prompt: &str, cancel: &CancellationToken) -> Result<String, AiProviderError>;
}

/// An AI provider capable of producing multi-token completion suggestions.
pub trait MultiTokenCompletionProvider: Send + Sync {
    fn complete_multi_token<'a>(
        &'a self,
        prompt: String,
        max_items: usize,
    ) -> BoxFuture<'a, Result<Vec<MultiTokenCompletion>, AiProviderError>>;
}
