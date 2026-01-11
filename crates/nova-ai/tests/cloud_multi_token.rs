use httpmock::prelude::*;
use nova_ai::{
    CloudLlmClient, CloudLlmConfig, CloudMultiTokenCompletionProvider, CompletionContextBuilder,
    MultiTokenCompletionContext, MultiTokenCompletionProvider, MultiTokenCompletionRequest,
    MultiTokenInsertTextFormat, PrivacyMode, ProviderKind, RetryConfig,
};
use serde_json::json;
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

fn provider_for_server(server: &MockServer) -> CloudMultiTokenCompletionProvider {
    let cfg = CloudLlmConfig {
        provider: ProviderKind::Http,
        endpoint: Url::parse(&format!("{}/complete", server.base_url())).unwrap(),
        api_key: None,
        model: "test-model".to_string(),
        timeout: Duration::from_secs(2),
        retry: RetryConfig {
            max_retries: 0,
            ..RetryConfig::default()
        },
        audit_logging: false,
        cache_enabled: false,
        cache_max_entries: 256,
        cache_ttl: Duration::from_secs(300),
    };
    let client = CloudLlmClient::new(cfg).unwrap();
    CloudMultiTokenCompletionProvider::new(client)
        .with_max_output_tokens(50)
        .with_temperature(0.1)
        .with_privacy_mode(PrivacyMode {
            anonymize_identifiers: false,
            include_file_paths: false,
            ..PrivacyMode::default()
        })
}

#[tokio::test]
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

#[tokio::test]
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

#[tokio::test]
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
