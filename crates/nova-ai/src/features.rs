use crate::{
    client::AiClient,
    types::{ChatMessage, ChatRequest, CodeSnippet},
    AiError,
};
use nova_config::AiConfig;
use tokio_util::sync::CancellationToken;

pub struct NovaAi {
    client: AiClient,
}

impl NovaAi {
    pub fn new(config: &AiConfig) -> Result<Self, AiError> {
        Ok(Self {
            client: AiClient::from_config(config)?,
        })
    }

    pub async fn explain_error(
        &self,
        error_message: &str,
        code_context: &CodeSnippet,
        cancel: CancellationToken,
    ) -> Result<String, AiError> {
        let context = self
            .client
            .sanitize_snippet(code_context)
            .unwrap_or_else(|| "[code context omitted due to excluded_paths]".to_string());

        let user_prompt = format!(
            "Explain this Java compiler/runtime error to a developer.\n\n\
             Error:\n{error_message}\n\n\
             Code context:\n{context}\n\n\
             Provide:\n\
             1) What the error means\n\
             2) Why it happened\n\
             3) How to fix it\n"
        );

        self.client
            .chat(
                ChatRequest {
                    messages: vec![
                        ChatMessage::system("You are a helpful Java assistant."),
                        ChatMessage::user(user_prompt),
                    ],
                    max_tokens: None,
                },
                cancel,
            )
            .await
    }

    pub async fn generate_method_body(
        &self,
        method_signature: &str,
        class_context: &CodeSnippet,
        cancel: CancellationToken,
    ) -> Result<String, AiError> {
        let context = self
            .client
            .sanitize_snippet(class_context)
            .unwrap_or_else(|| "[class context omitted due to excluded_paths]".to_string());
        let method_signature = self
            .client
            .sanitize_snippet(&CodeSnippet::ad_hoc(method_signature))
            .unwrap_or_else(|| "[method signature omitted due to excluded_paths]".to_string());

        let user_prompt = format!(
            "Given the following Java class context:\n{context}\n\n\
             Implement the method:\n{method_signature}\n\n\
             Return ONLY the method body (no signature), wrapped in braces.\n"
        );

        self.client
            .chat(
                ChatRequest {
                    messages: vec![
                        ChatMessage::system("You write correct, idiomatic Java code."),
                        ChatMessage::user(user_prompt),
                    ],
                    max_tokens: None,
                },
                cancel,
            )
            .await
    }

    pub async fn generate_tests(
        &self,
        method_or_class: &CodeSnippet,
        test_framework: Option<&str>,
        cancel: CancellationToken,
    ) -> Result<String, AiError> {
        let code = self
            .client
            .sanitize_snippet(method_or_class)
            .unwrap_or_else(|| "[code omitted due to excluded_paths]".to_string());

        let framework = test_framework.unwrap_or("JUnit 5");
        let user_prompt = format!(
            "Generate {framework} tests for the following Java code:\n\n{code}\n\n\
             Include:\n\
             - Normal cases\n\
             - Edge cases\n\
             - Error conditions\n"
        );

        self.client
            .chat(
                ChatRequest {
                    messages: vec![
                        ChatMessage::system("You are a meticulous Java test engineer."),
                        ChatMessage::user(user_prompt),
                    ],
                    max_tokens: None,
                },
                cancel,
            )
            .await
    }

    pub async fn code_review(
        &self,
        diff: &str,
        cancel: CancellationToken,
    ) -> Result<String, AiError> {
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
                        ChatMessage::user(format!(
                        "Review this code change:\n\n{diff}\n\n\
                         Consider correctness, performance, security, maintainability, and tests.\n"
                    )),
                    ],
                    max_tokens: None,
                },
                cancel,
            )
            .await?;

        Ok(review)
    }
}
