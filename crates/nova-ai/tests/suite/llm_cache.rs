use httpmock::prelude::*;
use nova_ai::{AiClient, ChatMessage, ChatRequest};
use nova_config::{AiConfig, AiProviderKind};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use url::Url;

fn http_config(url: Url, model: &str) -> AiConfig {
    let mut cfg = AiConfig::default();
    cfg.enabled = true;
    cfg.provider.kind = AiProviderKind::Http;
    cfg.provider.url = url;
    cfg.provider.model = model.to_string();
    cfg.provider.timeout_ms = 1_000;
    cfg.provider.concurrency = Some(1);
    cfg.provider.max_tokens = 64;
    cfg.privacy.local_only = false;
    cfg.privacy.anonymize_identifiers = Some(false);
    cfg.cache_enabled = true;
    cfg.cache_max_entries = 32;
    cfg.cache_ttl_secs = 60;
    cfg
}

#[tokio::test]
async fn llm_chat_is_cached_for_identical_requests() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/complete");
        then.status(200).json_body(json!({ "completion": "Pong" }));
    });

    let cfg = http_config(
        Url::parse(&format!("{}/complete", server.base_url())).unwrap(),
        "default",
    );

    let client = AiClient::from_config(&cfg).unwrap();
    let request = ChatRequest {
        messages: vec![ChatMessage::user("Ping")],
        max_tokens: Some(5),
        temperature: Some(0.2),
    };

    let out1 = client
        .chat(request.clone(), CancellationToken::new())
        .await
        .unwrap();
    let out2 = client
        .chat(request, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(out1, "Pong");
    assert_eq!(out2, "Pong");
    mock.assert_hits(1);
}

#[tokio::test]
async fn llm_cache_misses_when_model_changes() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/complete");
        then.status(200).json_body(json!({ "completion": "Pong" }));
    });

    let base_url = Url::parse(&format!("{}/complete", server.base_url())).unwrap();
    let request = ChatRequest {
        messages: vec![ChatMessage::user("Ping")],
        max_tokens: Some(5),
        temperature: Some(0.2),
    };

    let client_a = AiClient::from_config(&http_config(base_url.clone(), "model-a")).unwrap();
    assert_eq!(
        client_a
            .chat(request.clone(), CancellationToken::new())
            .await
            .unwrap(),
        "Pong"
    );

    let client_b = AiClient::from_config(&http_config(base_url, "model-b")).unwrap();
    assert_eq!(
        client_b
            .chat(request, CancellationToken::new())
            .await
            .unwrap(),
        "Pong"
    );

    // Both requests should hit the network because the model differs (keyed in the cache).
    mock.assert_hits(2);
}

#[tokio::test]
async fn llm_cache_misses_when_temperature_changes() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/complete");
        then.status(200).json_body(json!({ "completion": "Pong" }));
    });

    let cfg = http_config(
        Url::parse(&format!("{}/complete", server.base_url())).unwrap(),
        "default",
    );
    let client = AiClient::from_config(&cfg).unwrap();

    let request_a = ChatRequest {
        messages: vec![ChatMessage::user("Ping")],
        max_tokens: Some(5),
        temperature: Some(0.2),
    };
    let request_b = ChatRequest {
        temperature: Some(0.3),
        ..request_a.clone()
    };

    assert_eq!(
        client
            .chat(request_a, CancellationToken::new())
            .await
            .unwrap(),
        "Pong"
    );
    assert_eq!(
        client
            .chat(request_b, CancellationToken::new())
            .await
            .unwrap(),
        "Pong"
    );

    mock.assert_hits(2);
}

#[tokio::test]
async fn llm_cache_misses_when_temperature_is_unset_vs_zero() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/complete");
        then.status(200).json_body(json!({ "completion": "Pong" }));
    });

    let cfg = http_config(
        Url::parse(&format!("{}/complete", server.base_url())).unwrap(),
        "default",
    );
    let client = AiClient::from_config(&cfg).unwrap();

    let request_none = ChatRequest {
        messages: vec![ChatMessage::user("Ping")],
        max_tokens: Some(5),
        temperature: None,
    };
    let request_zero = ChatRequest {
        temperature: Some(0.0),
        ..request_none.clone()
    };

    assert_eq!(
        client
            .chat(request_none, CancellationToken::new())
            .await
            .unwrap(),
        "Pong"
    );
    assert_eq!(
        client
            .chat(request_zero, CancellationToken::new())
            .await
            .unwrap(),
        "Pong"
    );

    // The requests differ in how temperature is specified (unset vs explicitly zero).
    // These should not share a cache entry.
    mock.assert_hits(2);
}
