use crate::{
    actions,
    client::{AiClient, LlmClient},
    context::{BuiltContext, ContextBuilder, ContextRequest},
    types::{ChatMessage, ChatRequest, CodeSnippet},
    AiError,
};
use nova_config::AiConfig;
use std::{path::Path, sync::Arc};
use tokio_util::sync::CancellationToken;

pub struct NovaAi {
    client: Arc<AiClient>,
    context_builder: ContextBuilder,
    max_output_tokens: u32,
}

impl NovaAi {
    pub fn new(config: &AiConfig) -> Result<Self, AiError> {
        Ok(Self {
            client: Arc::new(AiClient::from_config(config)?),
            context_builder: ContextBuilder::new(),
            max_output_tokens: 512,
        })
    }

    pub fn with_max_output_tokens(mut self, max: u32) -> Self {
        self.max_output_tokens = max;
        self
    }

    fn maybe_omit_context(&self, ctx: &ContextRequest, built: BuiltContext) -> BuiltContext {
        let Some(path) = ctx.file_path.as_deref() else {
            return built;
        };

        // Best-effort: treat `file_path` as a filesystem path (callers should avoid URIs here so
        // excluded_paths glob matching works).
        if self.client.is_excluded_path(Path::new(path)) {
            return BuiltContext {
                text: "[code context omitted due to excluded_paths]".to_string(),
                token_count: 0,
                truncated: true,
            };
        }

        built
    }

    pub async fn explain_error(
        &self,
        diagnostic_message: &str,
        ctx: ContextRequest,
        cancel: CancellationToken,
    ) -> Result<String, AiError> {
        let built = self.context_builder.build(ctx.clone());
        let BuiltContext { text, .. } = self.maybe_omit_context(&ctx, built);

        let user_prompt = actions::explain_error_prompt(diagnostic_message, &text);
        self.client
            .chat(
                ChatRequest {
                    messages: vec![
                        ChatMessage::system("You are an expert Java developer assistant."),
                        ChatMessage::user(user_prompt),
                    ],
                    max_tokens: Some(self.max_output_tokens),
                    temperature: None,
                },
                cancel,
            )
            .await
    }

    pub async fn generate_method_body(
        &self,
        method_signature: &str,
        ctx: ContextRequest,
        cancel: CancellationToken,
    ) -> Result<String, AiError> {
        let built = self.context_builder.build(ctx.clone());
        let BuiltContext { text, .. } = self.maybe_omit_context(&ctx, built);

        let user_prompt = actions::generate_method_body_prompt(method_signature, &text);
        self.client
            .chat(
                ChatRequest {
                    messages: vec![
                        ChatMessage::system("You write correct, idiomatic Java code."),
                        ChatMessage::user(user_prompt),
                    ],
                    max_tokens: Some(self.max_output_tokens),
                    temperature: None,
                },
                cancel,
            )
            .await
    }

    pub async fn generate_tests(
        &self,
        target: &str,
        ctx: ContextRequest,
        cancel: CancellationToken,
    ) -> Result<String, AiError> {
        let built = self.context_builder.build(ctx.clone());
        let BuiltContext { text, .. } = self.maybe_omit_context(&ctx, built);

        let user_prompt = actions::generate_tests_prompt(target, &text);
        self.client
            .chat(
                ChatRequest {
                    messages: vec![
                        ChatMessage::system("You are a meticulous Java test engineer."),
                        ChatMessage::user(user_prompt),
                    ],
                    max_tokens: Some(self.max_output_tokens),
                    temperature: None,
                },
                cancel,
            )
            .await
    }

    pub async fn code_review(&self, diff: &str, cancel: CancellationToken) -> Result<String, AiError> {
        let diff = self
            .client
            .sanitize_snippet(&CodeSnippet::ad_hoc(diff))
            .unwrap_or_else(|| "[diff omitted due to excluded_paths]".to_string());

        let review = self
            .client
            .chat(
                ChatRequest {
                    messages: vec![
                        ChatMessage::system("You are a senior Java engineer doing code review."),
                        ChatMessage::user(actions::code_review_prompt(&diff)),
                    ],
                    max_tokens: Some(self.max_output_tokens),
                    temperature: None,
                },
                cancel,
            )
            .await?;

        Ok(review)
    }

    /// Access the underlying client (for model listing, custom prompts, etc).
    pub fn llm(&self) -> Arc<dyn LlmClient> {
        self.client.clone()
    }
}

