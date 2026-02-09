use crate::{
    actions,
    client::{AiClient, LlmClient},
    context::{BuiltContext, ContextBuilder, ContextRequest},
    diff,
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

    pub fn is_excluded_path(&self, path: &Path) -> bool {
        self.client.is_excluded_path(path)
    }

    fn sanitize_context_request_for_excluded_paths(
        &self,
        mut ctx: ContextRequest,
    ) -> ContextRequest {
        let mut omitted = false;

        ctx.extra_files.retain(|snippet| {
            let Some(path) = snippet.path.as_deref() else {
                return true;
            };
            if self.client.is_excluded_path(path) {
                omitted = true;
                return false;
            }
            true
        });

        ctx.related_code.retain(|related| {
            if self.client.is_excluded_path(&related.path) {
                omitted = true;
                return false;
            }
            true
        });

        if omitted {
            ctx.extra_files.push(CodeSnippet::ad_hoc(
                "[some context omitted due to excluded_paths]",
            ));
        }

        ctx
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
                sections: Vec::new(),
            };
        }

        built
    }

    fn explain_error_request(&self, diagnostic_message: &str, ctx: ContextRequest) -> ChatRequest {
        let ctx_sanitized = self.sanitize_context_request_for_excluded_paths(ctx.clone());
        let built = self.context_builder.build(ctx_sanitized);
        let BuiltContext { text, .. } = self.maybe_omit_context(&ctx, built);

        let user_prompt = actions::explain_error_prompt(diagnostic_message, &text);
        ChatRequest {
            messages: vec![
                ChatMessage::system("You are an expert Java developer assistant."),
                ChatMessage::user(user_prompt),
            ],
            max_tokens: Some(self.max_output_tokens),
            temperature: None,
        }
    }

    pub async fn explain_error(
        &self,
        diagnostic_message: &str,
        ctx: ContextRequest,
        cancel: CancellationToken,
    ) -> Result<String, AiError> {
        self.client
            .chat(self.explain_error_request(diagnostic_message, ctx), cancel)
            .await
    }

    fn generate_method_body_request(
        &self,
        method_signature: &str,
        ctx: ContextRequest,
    ) -> ChatRequest {
        let ctx_sanitized = self.sanitize_context_request_for_excluded_paths(ctx.clone());
        let built = self.context_builder.build(ctx_sanitized);
        let BuiltContext { text, .. } = self.maybe_omit_context(&ctx, built);

        let user_prompt = actions::generate_method_body_prompt(method_signature, &text);
        ChatRequest {
            messages: vec![
                ChatMessage::system("You write correct, idiomatic Java code."),
                ChatMessage::user(user_prompt),
            ],
            max_tokens: Some(self.max_output_tokens),
            temperature: None,
        }
    }

    pub async fn generate_method_body(
        &self,
        method_signature: &str,
        ctx: ContextRequest,
        cancel: CancellationToken,
    ) -> Result<String, AiError> {
        self.client
            .chat(
                self.generate_method_body_request(method_signature, ctx),
                cancel,
            )
            .await
    }

    fn generate_tests_request(&self, target: &str, ctx: ContextRequest) -> ChatRequest {
        let ctx_sanitized = self.sanitize_context_request_for_excluded_paths(ctx.clone());
        let built = self.context_builder.build(ctx_sanitized);
        let BuiltContext { text, .. } = self.maybe_omit_context(&ctx, built);

        let user_prompt = actions::generate_tests_prompt(target, &text);
        ChatRequest {
            messages: vec![
                ChatMessage::system("You are a meticulous Java test engineer."),
                ChatMessage::user(user_prompt),
            ],
            max_tokens: Some(self.max_output_tokens),
            temperature: None,
        }
    }

    pub async fn generate_tests(
        &self,
        target: &str,
        ctx: ContextRequest,
        cancel: CancellationToken,
    ) -> Result<String, AiError> {
        self.client
            .chat(self.generate_tests_request(target, ctx), cancel)
            .await
    }

    pub async fn code_review(
        &self,
        diff: &str,
        cancel: CancellationToken,
    ) -> Result<String, AiError> {
        self.code_review_with_llm(self.client.as_ref(), diff, cancel)
            .await
    }

    /// Like [`NovaAi::code_review`], but allows the caller (tests) to provide an alternate LLM
    /// client implementation.
    #[doc(hidden)]
    pub async fn code_review_with_llm(
        &self,
        llm: &dyn LlmClient,
        diff: &str,
        cancel: CancellationToken,
    ) -> Result<String, AiError> {
        let filtered = diff::filter_diff_for_excluded_paths(diff, |path| {
            self.client.is_excluded_path(path)
        });

        let sanitized = self
            .client
            .sanitize_snippet(&CodeSnippet::ad_hoc(filtered.text))
            .unwrap_or_else(|| diff::DIFF_OMITTED_PLACEHOLDER.to_string());
        let sanitized = diff::replace_omission_sentinels(&sanitized);

        llm.chat(
            ChatRequest {
                messages: vec![
                    ChatMessage::system("You are a senior Java engineer doing code review."),
                    ChatMessage::user(actions::code_review_prompt(&sanitized)),
                ],
                max_tokens: Some(self.max_output_tokens),
                temperature: None,
            },
            cancel,
        )
        .await
    }

    /// Access the underlying client (for model listing, custom prompts, etc).
    pub fn llm(&self) -> Arc<dyn LlmClient> {
        self.client.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        context::RelatedCode,
        privacy::{PrivacyMode, RedactionConfig},
    };
    use nova_config::AiPrivacyConfig;
    use std::path::PathBuf;

    #[test]
    fn excluded_paths_are_removed_from_related_code_and_extra_files_in_prompts() {
        let mut config = AiConfig::default();
        config.privacy = AiPrivacyConfig {
            excluded_paths: vec!["src/secrets/**".to_string()],
            ..AiPrivacyConfig::default()
        };

        let ai = NovaAi::new(&config).expect("NovaAi should build with dummy config");

        let secret_marker = "DO_NOT_LEAK_THIS_SECRET";
        let secret_code = format!("class Secret {{ String v = {secret_marker}; }}");
        let allowed_code = "class Helper {}".to_string();

        let ctx = ContextRequest {
            file_path: Some("src/Main.java".to_string()),
            focal_code: "class Main {}".to_string(),
            enclosing_context: None,
            project_context: None,
            semantic_context: None,
            related_symbols: Vec::new(),
            related_code: vec![
                RelatedCode {
                    path: PathBuf::from("src/secrets/Secret.java"),
                    range: 0..0,
                    kind: "class".to_string(),
                    snippet: secret_code.clone(),
                },
                RelatedCode {
                    path: PathBuf::from("src/Helper.java"),
                    range: 0..0,
                    kind: "class".to_string(),
                    snippet: allowed_code.clone(),
                },
            ],
            cursor: None,
            diagnostics: Vec::new(),
            extra_files: vec![
                CodeSnippet::new("src/secrets/Secret.java", secret_code.clone()),
                CodeSnippet::new("src/Helper.java", allowed_code.clone()),
            ],
            doc_comments: None,
            include_doc_comments: false,
            token_budget: 10_000,
            // Disable prompt-time anonymization/redaction so the test fails if the secret code is
            // included (we want omission, not masking).
            privacy: PrivacyMode {
                anonymize_identifiers: false,
                include_file_paths: true,
                redaction: RedactionConfig {
                    redact_string_literals: false,
                    redact_numeric_literals: false,
                    redact_comments: false,
                },
            },
        };

        let request = ai.explain_error_request("boom", ctx);
        let prompt = request
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            !prompt.contains(secret_marker),
            "excluded_paths code leaked into prompt: {prompt}"
        );
        assert!(
            prompt.contains("[some context omitted due to excluded_paths]"),
            "expected omission placeholder in prompt; got: {prompt}"
        );
        assert!(
            prompt.contains(&allowed_code),
            "expected allowed code to remain in prompt; got: {prompt}"
        );
    }
}
