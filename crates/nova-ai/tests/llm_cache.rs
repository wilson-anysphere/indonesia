use httpmock::prelude::*;
use nova_ai::cloud::GenerateRequest;
use nova_ai::{CloudLlmClient, CloudLlmConfig, ProviderKind, RetryConfig};
use serde_json::json;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use url::Url;

#[tokio::test]
async fn cloud_llm_generate_is_cached_for_identical_requests() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/complete");
        then.status(200).json_body(json!({ "completion": "Pong" }));
    });

    let cfg = CloudLlmConfig {
        provider: ProviderKind::Http,
        endpoint: Url::parse(&format!("{}/complete", server.base_url())).unwrap(),
        api_key: None,
        model: "default".to_string(),
        timeout: Duration::from_secs(1),
        retry: RetryConfig {
            max_retries: 0,
            ..RetryConfig::default()
        },
        audit_logging: false,
        cache_enabled: true,
        cache_max_entries: 32,
        cache_ttl: Duration::from_secs(60),
    };

    let client = CloudLlmClient::new(cfg).unwrap();
    let request = GenerateRequest {
        prompt: "Ping".to_string(),
        max_tokens: 5,
        temperature: 0.2,
    };

    let out1 = client
        .generate(request.clone(), CancellationToken::new())
        .await
        .unwrap();
    let out2 = client
        .generate(request, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(out1, "Pong");
    assert_eq!(out2, "Pong");
    mock.assert_hits(1);
}

#[tokio::test]
async fn cloud_llm_cache_misses_when_model_changes() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/complete");
        then.status(200).json_body(json!({ "completion": "Pong" }));
    });

    let base_cfg = CloudLlmConfig {
        provider: ProviderKind::Http,
        endpoint: Url::parse(&format!("{}/complete", server.base_url())).unwrap(),
        api_key: None,
        model: "model-a".to_string(),
        timeout: Duration::from_secs(1),
        retry: RetryConfig {
            max_retries: 0,
            ..RetryConfig::default()
        },
        audit_logging: false,
        cache_enabled: true,
        cache_max_entries: 32,
        cache_ttl: Duration::from_secs(60),
    };

    let request = GenerateRequest {
        prompt: "Ping".to_string(),
        max_tokens: 5,
        temperature: 0.2,
    };

    let client_a = CloudLlmClient::new(base_cfg.clone()).unwrap();
    let out_a = client_a
        .generate(request.clone(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(out_a, "Pong");

    let mut cfg_b = base_cfg;
    cfg_b.model = "model-b".to_string();
    let client_b = CloudLlmClient::new(cfg_b).unwrap();
    let out_b = client_b
        .generate(request, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(out_b, "Pong");

    // Both requests should hit the network because the model differs (keyed in the cache).
    mock.assert_hits(2);
}

#[tokio::test]
async fn cloud_llm_cache_misses_when_temperature_changes() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/complete");
        then.status(200).json_body(json!({ "completion": "Pong" }));
    });

    let cfg = CloudLlmConfig {
        provider: ProviderKind::Http,
        endpoint: Url::parse(&format!("{}/complete", server.base_url())).unwrap(),
        api_key: None,
        model: "default".to_string(),
        timeout: Duration::from_secs(1),
        retry: RetryConfig {
            max_retries: 0,
            ..RetryConfig::default()
        },
        audit_logging: false,
        cache_enabled: true,
        cache_max_entries: 32,
        cache_ttl: Duration::from_secs(60),
    };

    let client = CloudLlmClient::new(cfg).unwrap();
    let request_a = GenerateRequest {
        prompt: "Ping".to_string(),
        max_tokens: 5,
        temperature: 0.2,
    };
    let request_b = GenerateRequest {
        temperature: 0.3,
        ..request_a.clone()
    };

    let out1 = client
        .generate(request_a, CancellationToken::new())
        .await
        .unwrap();
    let out2 = client
        .generate(request_b, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(out1, "Pong");
    assert_eq!(out2, "Pong");
    mock.assert_hits(2);
}
