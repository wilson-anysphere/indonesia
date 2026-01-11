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
        let context = if code_context
            .path
            .as_deref()
            .is_some_and(|path| self.client.is_excluded_path(path))
        {
            "[code context omitted due to excluded_paths]".to_string()
        } else {
            code_context.content.clone()
        };

        let user_prompt = format!(
            "Explain this Java compiler/runtime error to a developer.\n\n\
             Error:\n{error_message}\n\n\
             Code context:\n```java\n{context}\n```\n\n\
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
        let context = if class_context
            .path
            .as_deref()
            .is_some_and(|path| self.client.is_excluded_path(path))
        {
            "[class context omitted due to excluded_paths]".to_string()
        } else {
            class_context.content.clone()
        };

        let user_prompt = format!(
            "Given the following Java class context:\n```java\n{context}\n```\n\n\
             Implement the method:\n```java\n{method_signature}\n```\n\n\
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
        let code = if method_or_class
            .path
            .as_deref()
            .is_some_and(|path| self.client.is_excluded_path(path))
        {
            "[code omitted due to excluded_paths]".to_string()
        } else {
            method_or_class.content.clone()
        };

        let framework = test_framework.unwrap_or("JUnit 5");
        let user_prompt = format!(
            "Generate {framework} tests for the following Java code:\n\n```java\n{code}\n```\n\n\
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
        let diff = diff.to_string();

        let review = self
            .client
            .chat(
                ChatRequest {
                    messages: vec![
                        ChatMessage::system("You are a senior Java engineer doing code review."),
                        ChatMessage::user(format!(
                        "Review this code change:\n\n```diff\n{diff}\n```\n\n\
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
