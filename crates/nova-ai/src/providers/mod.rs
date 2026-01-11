#[cfg(feature = "local-llm")]
pub mod in_process_llama;
pub mod ollama;
pub mod openai_compatible;

use crate::{types::AiStream, AiError, CancellationToken, ChatRequest};
use async_trait::async_trait;

#[async_trait]
pub trait AiProvider: Send + Sync {
    async fn chat(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<String, AiError>;

    async fn chat_stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<AiStream, AiError>;

    async fn list_models(&self, cancel: CancellationToken) -> Result<Vec<String>, AiError>;
}
