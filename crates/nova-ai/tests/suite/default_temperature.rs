use httpmock::prelude::*;
use nova_ai::{AiClient, ChatMessage, ChatRequest};
use nova_config::{AiConfig, AiProviderKind};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use url::Url;

fn http_config(url: Url) -> AiConfig {
    let mut cfg = AiConfig::default();
    cfg.enabled = true;
    cfg.provider.kind = AiProviderKind::Http;
    cfg.provider.url = url;
    cfg.provider.model = "test-model".to_string();
    cfg.provider.timeout_ms = 1_000;
    cfg.provider.concurrency = Some(1);
    cfg.provider.max_tokens = 64;
    cfg.privacy.local_only = false;
    cfg.privacy.anonymize_identifiers = Some(false);
    cfg.cache_enabled = false;
    cfg
}

#[tokio::test]
async fn default_temperature_is_sent_when_configured() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST)
            .path("/complete")
            .body_contains("\"temperature\":0.2");
        then.status(200).json_body(json!({ "completion": "Pong" }));
    });

    let mut cfg = http_config(
        Url::parse(&format!("{}/complete", server.base_url())).expect("valid server url"),
    );
    cfg.provider.temperature = Some(0.2);

    let client = AiClient::from_config(&cfg).expect("client");
    let out = client
        .chat(
            ChatRequest {
                messages: vec![ChatMessage::user("Ping")],
                max_tokens: Some(5),
                temperature: None,
            },
            CancellationToken::new(),
        )
        .await
        .expect("chat succeeds");
    assert_eq!(out, "Pong");

    mock.assert_hits(1);
}

#[tokio::test]
async fn temperature_field_is_omitted_when_unset() {
    let server = MockServer::start();
    let expected_body = json!({
        "model": "test-model",
        "prompt": "User:\nPing",
        "max_tokens": 5,
    });
    let mock = server.mock(|when, then| {
        when.method(POST).path("/complete").json_body(expected_body);
        then.status(200).json_body(json!({ "completion": "Pong" }));
    });

    let cfg = http_config(
        Url::parse(&format!("{}/complete", server.base_url())).expect("valid server url"),
    );

    let client = AiClient::from_config(&cfg).expect("client");
    let out = client
        .chat(
            ChatRequest {
                messages: vec![ChatMessage::user("Ping")],
                max_tokens: Some(5),
                temperature: None,
            },
            CancellationToken::new(),
        )
        .await
        .expect("chat succeeds");
    assert_eq!(out, "Pong");

    mock.assert_hits(1);
}
