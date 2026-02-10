use httpmock::prelude::*;
use nova_ai::{
    AdditionalEdit, AiClient, AiError, AiStream, ChatRequest, CloudMultiTokenCompletionProvider,
    CompletionContextBuilder, LlmClient, MultiTokenCompletionContext, MultiTokenCompletionProvider,
    MultiTokenCompletionRequest, MultiTokenInsertTextFormat, PrivacyMode, RedactionConfig,
};
use nova_config::{AiConfig, AiProviderKind};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
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

fn extract_first_section_bullet(prompt: &str, section_header: &str) -> String {
    let mut lines = prompt.lines();
    while let Some(line) = lines.next() {
        if line.trim() != section_header {
            continue;
        }

        for line in lines.by_ref() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(value) = line.strip_prefix("- ") {
                let value = value.trim();
                if value.is_empty() {
                    continue;
                }
                return value.to_string();
            }

            // Reached a different section before finding a bullet.
            if line.ends_with(':') {
                break;
            }
        }

        break;
    }

    panic!("missing bullet under section {section_header:?}\n{prompt}");
}

#[derive(Default)]
struct AnonymizationRoundTripLlm {
    calls: AtomicUsize,
    prompt: std::sync::Mutex<Option<String>>,
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
async fn cloud_multi_token_round_trips_identifier_anonymization_and_deanonymization() {
    let llm = Arc::new(AnonymizationRoundTripLlm::default());
    let provider =
        CloudMultiTokenCompletionProvider::new(llm.clone()).with_privacy_mode(PrivacyMode {
            anonymize_identifiers: true,
            include_file_paths: false,
            ..PrivacyMode::default()
        });

    let ctx = MultiTokenCompletionContext {
        receiver_type: Some("java.util.Optional<String>".into()),
        expected_type: Some("java.lang.String".into()),
        surrounding_code: "System.out.".into(),
        available_methods: vec!["getSecretToken".into(), "filter".into()],
        importable_paths: vec!["com.example.SecretTokenProvider".into()],
    };
    let prompt = CompletionContextBuilder::new(10_000).build_completion_prompt(&ctx, 3);

    let out = provider
        .complete_multi_token(MultiTokenCompletionRequest {
            prompt,
            max_items: 3,
            timeout: Duration::from_secs(1),
            cancel: CancellationToken::new(),
        })
        .await
        .expect("provider call succeeds");

    assert_eq!(
        llm.calls.load(Ordering::SeqCst),
        1,
        "expected an LLM call when anonymization is enabled"
    );

    let captured = llm
        .prompt
        .lock()
        .expect("prompt mutex")
        .clone()
        .expect("captured prompt");
    assert!(
        !captured.contains("getSecretToken"),
        "prompt should not contain raw method identifier\n{captured}"
    );
    assert!(
        captured.contains("- filter"),
        "expected common JDK method identifier to remain readable in Available methods list\n{captured}"
    );
    assert!(
        !captured.contains("com.example.SecretTokenProvider"),
        "prompt should not contain raw import identifier\n{captured}"
    );
    assert!(
        captured.contains("id_"),
        "expected anonymized id_ placeholders in prompt\n{captured}"
    );

    assert_eq!(out.len(), 1);
    assert_eq!(out[0].insert_text, "getSecretToken()");
    assert_eq!(
        out[0].additional_edits,
        vec![AdditionalEdit::AddImport {
            path: "com.example.SecretTokenProvider".to_string()
        }]
    );
    assert!(
        !out[0].insert_text.contains("id_"),
        "expected insert_text to be de-anonymized\n{:?}",
        out[0]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn cloud_multi_token_clamps_after_deanonymization() {
    let llm = Arc::new(AnonymizationRoundTripLlm::default());
    let provider = CloudMultiTokenCompletionProvider::new(llm)
        .with_max_insert_text_chars(8)
        .with_privacy_mode(PrivacyMode {
            anonymize_identifiers: true,
            include_file_paths: false,
            ..PrivacyMode::default()
        });

    let ctx = MultiTokenCompletionContext {
        receiver_type: Some("java.util.Optional<String>".into()),
        expected_type: Some("java.lang.String".into()),
        surrounding_code: "System.out.".into(),
        available_methods: vec!["getSecretToken".into()],
        importable_paths: vec!["com.example.SecretTokenProvider".into()],
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
        out[0].insert_text.starts_with("get"),
        "expected insert_text to be de-anonymized\n{:?}",
        out[0]
    );
    assert!(
        out[0].insert_text.chars().count() <= 8,
        "expected insert_text to be clamped after deanonymization (got {})\n{:?}",
        out[0].insert_text.chars().count(),
        out[0]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn deanonymization_does_not_rewrite_inside_strings_or_comments() {
    let llm = Arc::new(StringsAndCommentsRoundTripLlm::default());

    let provider =
        CloudMultiTokenCompletionProvider::new(llm.clone()).with_privacy_mode(PrivacyMode {
            anonymize_identifiers: true,
            include_file_paths: false,
            ..PrivacyMode::default()
        });

    let ctx = MultiTokenCompletionContext {
        receiver_type: Some("java.util.Optional<String>".into()),
        expected_type: Some("java.lang.String".into()),
        surrounding_code: "System.out.".into(),
        available_methods: vec!["getSecretToken".into()],
        importable_paths: vec!["com.example.SecretTokenProvider".into()],
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

    let captured = llm
        .prompt
        .lock()
        .expect("prompt mutex")
        .clone()
        .expect("captured prompt");
    let method_token = extract_first_section_bullet(&captured, "Available methods:");

    assert_eq!(out.len(), 1);
    assert_eq!(
        out[0].insert_text,
        format!(
            "String s = \"{method_token}\"; // {method_token}\ngetSecretToken(); /* {method_token} */"
        )
    );
    assert_eq!(
        out[0].additional_edits,
        vec![AdditionalEdit::AddImport {
            path: "com.example.SecretTokenProvider".to_string()
        }]
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
struct StringsAndCommentsRoundTripLlm {
    prompt: std::sync::Mutex<Option<String>>,
}

#[async_trait::async_trait]
impl LlmClient for StringsAndCommentsRoundTripLlm {
    async fn chat(
        &self,
        request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<String, AiError> {
        let prompt = request
            .messages
            .first()
            .map(|msg| msg.content.clone())
            .unwrap_or_default();

        let method_token = extract_first_section_bullet(&prompt, "Available methods:");
        let import_token = extract_first_section_bullet(&prompt, "Importable symbols:");
        let insert_text = format!(
            "String s = \"{method_token}\"; // {method_token}\n{method_token}(); /* {method_token} */"
        );

        *self.prompt.lock().expect("prompt mutex") = Some(prompt);

        Ok(json!({
            "completions": [{
                "label": method_token,
                "insert_text": insert_text,
                "format": "plain",
                "additional_edits": [{"add_import": import_token}],
                "confidence": 1.0,
            }]
        })
        .to_string())
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

#[async_trait::async_trait]
impl LlmClient for AnonymizationRoundTripLlm {
    async fn chat(
        &self,
        request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<String, AiError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let prompt = request
            .messages
            .first()
            .map(|msg| msg.content.clone())
            .unwrap_or_default();

        assert!(
            !prompt.contains("getSecretToken"),
            "expected prompt to not leak raw method name\n{prompt}"
        );
        assert!(
            !prompt.contains("com.example.SecretTokenProvider"),
            "expected prompt to not leak raw import path\n{prompt}"
        );
        assert!(
            prompt.contains("id_"),
            "expected prompt to contain anonymized placeholders\n{prompt}"
        );

        let method_token = extract_first_section_bullet(&prompt, "Available methods:");
        let import_token = extract_first_section_bullet(&prompt, "Importable symbols:");
        assert!(
            method_token.contains("id_"),
            "expected available method to be anonymized\n{prompt}"
        );
        assert!(
            import_token.contains("id_"),
            "expected import path to be anonymized\n{prompt}"
        );

        *self.prompt.lock().expect("prompt mutex") = Some(prompt);

        Ok(json!({
            "completions": [{
                "label": "anon_round_trip",
                "insert_text": format!("{method_token}()"),
                "format": "plain",
                "additional_edits": [{"add_import": import_token}],
                "confidence": 1.0,
            }]
        })
        .to_string())
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
