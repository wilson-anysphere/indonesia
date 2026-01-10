use crate::cancel::CancellationToken;
use thiserror::Error;

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

