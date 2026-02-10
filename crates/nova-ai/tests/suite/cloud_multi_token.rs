use httpmock::prelude::*;
use nova_ai::{
    AiClient, AiError, AiStream, ChatRequest, CloudMultiTokenCompletionProvider,
    CompletionContextBuilder, LlmClient, MultiTokenCompletionContext, MultiTokenCompletionProvider,
    MultiTokenCompletionRequest, MultiTokenInsertTextFormat, PrivacyMode, RedactionConfig,
};
use nova_config::{AiConfig, AiProviderKind};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use url::Url;

fn ctx() -> MultiTokenCompletionContext {
    MultiTokenCompletionContext {
        receiver_type: Some("Stream<Person>".into()),
        expected_type: Some("List<String>".into()),
        surrounding_code: "people.stream().".into(),
        available_methods: vec!["filter".into(), "map".into(), "collect".into()],
        importable_paths: vec!["java.util.stream.Collectors".into()],
    }
}

fn http_config(server: &MockServer) -> AiConfig {
    let mut cfg = AiConfig::default();
    cfg.enabled = true;
    cfg.provider.kind = AiProviderKind::Http;
    cfg.provider.url = Url::parse(&format!("{}/complete", server.base_url())).unwrap();
    cfg.provider.model = "test-model".to_string();
    cfg.provider.timeout_ms = Duration::from_secs(2).as_millis() as u64;
    cfg.provider.concurrency = Some(1);
    cfg.privacy.local_only = false;
    cfg.privacy.anonymize_identifiers = Some(false);
    cfg.cache_enabled = false;
    cfg
}

fn provider_for_server(server: &MockServer) -> CloudMultiTokenCompletionProvider {
    let cfg = http_config(server);
    let client = Arc::new(AiClient::from_config(&cfg).unwrap());
    CloudMultiTokenCompletionProvider::new(client)
        .with_max_output_tokens(50)
        .with_temperature(0.1)
        .with_privacy_mode(PrivacyMode {
            anonymize_identifiers: false,
            include_file_paths: false,
            ..PrivacyMode::default()
        })
}

#[tokio::test(flavor = "current_thread")]
async fn sends_prompt_with_context_and_parses_raw_json() {
    let server = MockServer::start();

    let completion_payload = r#"{"completions":[{"label":"chain","insert_text":"filter(x -> true)","format":"plain","additional_edits":[],"confidence":0.9}]}"#;

    let mock = server.mock(|when, then| {
        when.method(POST)
            .path("/complete")
            .body_contains("Receiver type: Stream<Person>")
            .body_contains("Expected type: List<String>")
            .body_contains("Available methods:")
            .body_contains("Surrounding code:")
            .body_contains("Return JSON only.")
            .body_contains("\"max_tokens\":50")
            .body_contains("\"temperature\":0.1");
        then.status(200)
            .json_body(json!({ "completion": completion_payload }));
    });

    let provider = provider_for_server(&server);
    let prompt = CompletionContextBuilder::new(10_000).build_completion_prompt(&ctx(), 3);

    let out = provider
        .complete_multi_token(MultiTokenCompletionRequest {
            prompt,
            max_items: 3,
            timeout: Duration::from_secs(1),
            cancel: CancellationToken::new(),
        })
        .await
        .expect("provider call succeeds");
    mock.assert();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].label, "chain");
    assert_eq!(out[0].insert_text, "filter(x -> true)");
    assert_eq!(out[0].format, MultiTokenInsertTextFormat::PlainText);
    assert!((out[0].confidence - 0.9).abs() < f32::EPSILON);
}

#[tokio::test(flavor = "current_thread")]
async fn anonymizes_prompt_and_deanonymizes_completion_when_identifier_anonymization_is_enabled() {
    let secret = "SecretIdentifier";
    let llm = Arc::new(PromptEchoLlm::default());

    let provider = CloudMultiTokenCompletionProvider::new(llm.clone()).with_privacy_mode(
        PrivacyMode {
            anonymize_identifiers: true,
            include_file_paths: false,
            ..PrivacyMode::default()
        },
    );

    let ctx = MultiTokenCompletionContext {
        receiver_type: Some(secret.into()),
        expected_type: Some(secret.into()),
        surrounding_code: format!("{secret}."),
        available_methods: vec![secret.into()],
        importable_paths: vec![format!("java.util.{secret}")],
    };
    let prompt = CompletionContextBuilder::new(10_000).build_completion_prompt(&ctx, 1);

    let out = provider
        .complete_multi_token(MultiTokenCompletionRequest {
            prompt,
            max_items: 1,
            timeout: Duration::from_secs(1),
            cancel: CancellationToken::new(),
        })
        .await
        .expect("provider call succeeds");

    assert_eq!(out.len(), 1);
    assert_eq!(out[0].label, secret);
    assert_eq!(out[0].insert_text, secret);
    assert_eq!(out[0].format, MultiTokenInsertTextFormat::PlainText);
    assert_eq!(
        out[0].additional_edits,
        vec![nova_ai::AdditionalEdit::AddImport {
            path: format!("java.util.{secret}")
        }]
    );

    let captured = llm
        .prompt
        .lock()
        .expect("prompt mutex")
        .clone()
        .expect("captured prompt");
    assert!(
        !captured.contains(secret),
        "expected prompt to be anonymized; leaked identifier: {captured}"
    );
    assert!(
        captured.contains("Receiver type: id_0"),
        "expected receiver type to be anonymized: {captured}"
    );
    assert!(
        captured.contains("Expected type: id_0"),
        "expected expected type to be anonymized: {captured}"
    );
    assert!(
        captured.contains("- id_0"),
        "expected available method to be anonymized: {captured}"
    );
    assert!(
        captured.contains("- java.util.id_0"),
        "expected importable symbol to be anonymized: {captured}"
    );
    assert!(
        captured.contains("```java\nid_0.\n```"),
        "expected surrounding code to be anonymized: {captured}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn reapplies_clamps_after_deanonymization() {
    let long_secret = "SecretIdentifierWithAVeryLongNameThatShouldBeClamped_".repeat(4);
    assert!(long_secret.chars().count() > 120);

    let llm = Arc::new(PromptEchoLlm::default());
    let provider = CloudMultiTokenCompletionProvider::new(llm).with_privacy_mode(PrivacyMode {
        anonymize_identifiers: true,
        include_file_paths: false,
        ..PrivacyMode::default()
    });

    // Force a tiny insert_text clamp so the anonymized completion ("id_0") fits, but the
    // de-anonymized completion would exceed the limit unless we clamp again.
    let provider = provider.with_max_insert_text_chars(16);

    let ctx = MultiTokenCompletionContext {
        receiver_type: Some(long_secret.clone()),
        expected_type: Some(long_secret.clone()),
        surrounding_code: format!("{long_secret}."),
        available_methods: vec![long_secret.clone()],
        importable_paths: vec![format!("java.util.{long_secret}")],
    };
    let prompt = CompletionContextBuilder::new(10_000).build_completion_prompt(&ctx, 1);

    let out = provider
        .complete_multi_token(MultiTokenCompletionRequest {
            prompt,
            max_items: 1,
            timeout: Duration::from_secs(1),
            cancel: CancellationToken::new(),
        })
        .await
        .expect("provider call succeeds");

    assert_eq!(out.len(), 1);
    assert!(
        out[0].label.chars().count() <= 120,
        "label should be clamped after de-anonymization"
    );
    assert!(
        out[0].insert_text.chars().count() <= 16,
        "insert_text should be clamped after de-anonymization"
    );
    assert!(
        !out[0].label.contains("id_"),
        "expected label to be de-anonymized; got: {:?}",
        out[0].label
    );
    assert!(
        !out[0].insert_text.contains("id_"),
        "expected insert_text to be de-anonymized; got: {:?}",
        out[0].insert_text
    );
}

#[tokio::test(flavor = "current_thread")]
async fn parses_json_wrapped_in_fenced_block() {
    let server = MockServer::start();
    let completion_payload = r#"{"completions":[{"label":"fenced","insert_text":"map(x -> x)","format":"plain","additional_edits":[],"confidence":0.7}]}"#;
    let wrapped = format!("Here you go:\n```json\n{completion_payload}\n```\n");
    let mock = server.mock(|when, then| {
        when.method(POST).path("/complete");
        then.status(200).json_body(json!({ "completion": wrapped }));
    });

    let provider = provider_for_server(&server);
    let prompt = CompletionContextBuilder::new(10_000).build_completion_prompt(&ctx(), 3);

    let out = provider
        .complete_multi_token(MultiTokenCompletionRequest {
            prompt,
            max_items: 3,
            timeout: Duration::from_secs(1),
            cancel: CancellationToken::new(),
        })
        .await
        .expect("provider call succeeds");
    mock.assert();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].label, "fenced");
    assert_eq!(out[0].insert_text, "map(x -> x)");
    assert_eq!(out[0].format, MultiTokenInsertTextFormat::PlainText);
    assert!((out[0].confidence - 0.7).abs() < f32::EPSILON);
}

#[tokio::test(flavor = "current_thread")]
async fn invalid_json_gracefully_degrades_to_empty() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/complete");
        then.status(200)
            .json_body(json!({ "completion": "not json" }));
    });
    let provider = provider_for_server(&server);
    let prompt = CompletionContextBuilder::new(10_000).build_completion_prompt(&ctx(), 3);

    let out = provider
        .complete_multi_token(MultiTokenCompletionRequest {
            prompt,
            max_items: 3,
            timeout: Duration::from_secs(1),
            cancel: CancellationToken::new(),
        })
        .await
        .expect("provider call succeeds");
    mock.assert();
    assert!(out.is_empty());
}

#[derive(Default)]
struct CapturingLlm {
    prompt: std::sync::Mutex<Option<String>>,
}

#[async_trait::async_trait]
impl LlmClient for CapturingLlm {
    async fn chat(
        &self,
        request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<String, AiError> {
        let content = request
            .messages
            .first()
            .map(|msg| msg.content.clone())
            .unwrap_or_default();
        *self.prompt.lock().expect("prompt mutex") = Some(content);

        Ok(r#"{"completions":[{"label":"x","insert_text":"y","format":"plain","additional_edits":[],"confidence":0.5}]}"#.to_string())
    }

    async fn chat_stream(
        &self,
        _request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<AiStream, AiError> {
        Err(AiError::UnexpectedResponse(
            "streaming not supported in test".into(),
        ))
    }

    async fn list_models(&self, _cancel: CancellationToken) -> Result<Vec<String>, AiError> {
        Ok(Vec::new())
    }
}

#[derive(Default)]
struct PromptEchoLlm {
    prompt: std::sync::Mutex<Option<String>>,
}

#[async_trait::async_trait]
impl LlmClient for PromptEchoLlm {
    async fn chat(
        &self,
        request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<String, AiError> {
        let content = request
            .messages
            .first()
            .map(|msg| msg.content.clone())
            .unwrap_or_default();
        *self.prompt.lock().expect("prompt mutex") = Some(content);

        // The prompt is anonymized in privacy mode, so we can safely return placeholders and let
        // the provider de-anonymize before returning to the IDE.
        Ok(r#"{"completions":[{"label":"id_0","insert_text":"id_0","format":"plain","additional_edits":[{"add_import":"java.util.id_0"}],"confidence":0.5}]}"#.to_string())
    }

    async fn chat_stream(
        &self,
        _request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<AiStream, AiError> {
        Err(AiError::UnexpectedResponse(
            "streaming not supported in test".into(),
        ))
    }

    async fn list_models(&self, _cancel: CancellationToken) -> Result<Vec<String>, AiError> {
        Ok(Vec::new())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn sanitization_respects_comment_redaction_flag() {
    let llm = Arc::new(CapturingLlm::default());

    let provider =
        CloudMultiTokenCompletionProvider::new(llm.clone()).with_privacy_mode(PrivacyMode {
            anonymize_identifiers: false,
            include_file_paths: false,
            redaction: RedactionConfig {
                redact_string_literals: false,
                redact_numeric_literals: false,
                redact_comments: false,
            },
        });

    let ctx = MultiTokenCompletionContext {
        receiver_type: Some("Stream<Person>".into()),
        expected_type: Some("List<String>".into()),
        surrounding_code: "// KEEP_ME\npeople.stream().".into(),
        available_methods: vec!["filter".into(), "map".into(), "collect".into()],
        importable_paths: vec![],
    };

    let prompt = CompletionContextBuilder::new(10_000).build_completion_prompt(&ctx, 1);
    let out = provider
        .complete_multi_token(MultiTokenCompletionRequest {
            prompt,
            max_items: 1,
            timeout: Duration::from_secs(1),
            cancel: CancellationToken::new(),
        })
        .await
        .expect("provider call succeeds");
    assert_eq!(out.len(), 1);

    let captured = llm
        .prompt
        .lock()
        .expect("prompt mutex")
        .clone()
        .expect("captured prompt");
    assert!(
        captured.contains("KEEP_ME"),
        "expected comment content to be preserved when redact_comments=false\n{captured}"
    );
    assert!(
        !captured.contains("// [REDACTED]"),
        "expected comments not to be stripped when redact_comments=false\n{captured}"
    );
}
