use crate::cloud::{CloudLlmClient, CloudLlmError, GenerateRequest};
use crate::context::{BuiltContext, ContextBuilder, ContextRequest};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct AiService {
    llm: CloudLlmClient,
    context_builder: ContextBuilder,
    max_output_tokens: u32,
    temperature: f32,
}

impl AiService {
    pub fn new(llm: CloudLlmClient) -> Self {
        Self {
            llm,
            context_builder: ContextBuilder::new(),
            max_output_tokens: 512,
            temperature: 0.2,
        }
    }

    pub fn with_max_output_tokens(mut self, max: u32) -> Self {
        self.max_output_tokens = max;
        self
    }

    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.temperature = temperature;
        self
    }

    pub async fn explain_error(
        &self,
        diagnostic_message: &str,
        ctx: ContextRequest,
        cancel: CancellationToken,
    ) -> Result<String, CloudLlmError> {
        let BuiltContext { text, .. } = self.context_builder.build(ctx);
        let prompt = format!(
            "You are an expert Java developer assistant.\n\
             Explain the following compiler error in plain language.\n\n\
             Error:\n{diagnostic_message}\n\n\
             Code context:\n{text}\n\n\
             Provide:\n\
             1) What the error means\n\
             2) Why it happened\n\
             3) How to fix it\n"
        );

        self.llm
            .generate(
                GenerateRequest {
                    prompt,
                    max_tokens: self.max_output_tokens,
                    temperature: self.temperature,
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
    ) -> Result<String, CloudLlmError> {
        let BuiltContext { text, .. } = self.context_builder.build(ctx);
        let prompt = format!(
            "You are an expert Java developer assistant.\n\
             Implement the following Java method.\n\n\
             Method signature:\n{method_signature}\n\n\
             Context:\n{text}\n\n\
             Return ONLY the method body contents (no surrounding braces, no markdown).\n"
        );

        self.llm
            .generate(
                GenerateRequest {
                    prompt,
                    max_tokens: self.max_output_tokens,
                    temperature: self.temperature,
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
    ) -> Result<String, CloudLlmError> {
        let BuiltContext { text, .. } = self.context_builder.build(ctx);
        let prompt = format!(
            "You are an expert Java developer assistant.\n\
             Generate unit tests (JUnit 5) for the following target.\n\n\
             Target:\n{target}\n\n\
             Context:\n{text}\n\n\
             Include tests for normal cases, edge cases, and error conditions.\n\
             Return ONLY Java code (no markdown).\n"
        );

        self.llm
            .generate(
                GenerateRequest {
                    prompt,
                    max_tokens: self.max_output_tokens,
                    temperature: self.temperature,
                },
                cancel,
            )
            .await
    }
}

