//! Embedders that call external providers.
//!
//! These are separate from the async LLM provider implementations because
//! [`crate::EmbeddingSemanticSearch`] is synchronous (it is used from sync
//! contexts like `nova-ide` and plain `#[test]` unit tests). Provider-backed
//! embedders therefore must not rely on a caller-provided tokio runtime.

mod openai_compatible;

pub use openai_compatible::OpenAiCompatibleEmbedder;

