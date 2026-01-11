use futures::future::BoxFuture;
use std::time::Duration;
use thiserror::Error;

use crate::CancellationToken;
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
    fn complete(&self, prompt: &str, cancel: &CancellationToken) -> Result<String, AiProviderError>;
}

#[derive(Clone, Debug)]
pub struct MultiTokenCompletionRequest {
    pub prompt: String,
    pub max_items: usize,
    pub timeout: Duration,
    pub cancel: CancellationToken,
}

/// An AI provider capable of producing multi-token completion suggestions.
pub trait MultiTokenCompletionProvider: Send + Sync {
    fn complete_multi_token<'a>(
        &'a self,
        request: MultiTokenCompletionRequest,
    ) -> BoxFuture<'a, Result<Vec<MultiTokenCompletion>, AiProviderError>>;
}
