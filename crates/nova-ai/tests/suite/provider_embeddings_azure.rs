#![cfg(feature = "embeddings")]

use httpmock::prelude::*;
use nova_ai::embeddings::{embeddings_client_from_config, EmbeddingInputKind};
use nova_config::{AiConfig, AiEmbeddingsBackend, AiProviderKind};
use serde_json::json;
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;
use url::Url;

#[tokio::test(flavor = "current_thread")]
async fn azure_openai_provider_embeddings_hits_deployment_embeddings_endpoint() {
    let server = MockServer::start();

    let mock = server.mock(|when, then| {
        when.method(POST)
            .path("/openai/deployments/my-deployment/embeddings")
            .query_param("api-version", "2024-02-01")
            .header("api-key", "test-key");
        then.status(200).json_body(json!({
            "object": "list",
            "data": [
                {
                    "object": "embedding",
                    "embedding": [1.0, 2.0, 3.0],
                    "index": 0
                }
            ],
            "model": "ignored",
            "usage": { "prompt_tokens": 1, "total_tokens": 1 }
        }));
    });

    let mut config = AiConfig::default();
    config.enabled = true;
    config.embeddings.enabled = true;
    config.embeddings.backend = AiEmbeddingsBackend::Provider;
    config.provider.kind = AiProviderKind::AzureOpenAi;
    config.provider.url = Url::parse(&format!("{}/", server.base_url())).unwrap();
    config.provider.azure_deployment = Some("my-deployment".to_string());
    config.api_key = Some("test-key".to_string());
    config.privacy.local_only = false;

    let client = embeddings_client_from_config(&config).expect("build embeddings client");
    let out = client
        .embed(
            &["hello".to_string()],
            EmbeddingInputKind::Query,
            CancellationToken::new(),
        )
        .await
        .expect("embed");

    mock.assert();
    assert_eq!(out, vec![vec![1.0, 2.0, 3.0]]);
}

#[tokio::test(flavor = "current_thread")]
async fn azure_openai_provider_embeddings_redacts_absolute_paths_in_cloud_mode() {
    let server = MockServer::start();

    let dir = tempdir().expect("tempdir");
    let dir = dir.path().canonicalize().expect("canonicalize tempdir");
    let abs_path = dir.join("src").join("example.txt");
    let abs_path_str = abs_path.to_string_lossy().to_string();
    let abs_path_in_body = json_string_contents(&abs_path_str);

    let leaky = server.mock(|when, then| {
        when.method(POST)
            .path("/openai/deployments/my-deployment/embeddings")
            .query_param("api-version", "2024-02-01")
            .header("api-key", "test-key")
            .body_contains(&abs_path_in_body);
        then.status(500).json_body(json!({
            "error": "absolute path leaked to provider"
        }));
    });

    let redacted = server.mock(|when, then| {
        when.method(POST)
            .path("/openai/deployments/my-deployment/embeddings")
            .query_param("api-version", "2024-02-01")
            .header("api-key", "test-key")
            .body_contains("[PATH]");
        then.status(200).json_body(json!({
            "object": "list",
            "data": [
                {
                    "object": "embedding",
                    "embedding": [1.0, 2.0, 3.0],
                    "index": 0
                }
            ],
            "model": "ignored",
            "usage": { "prompt_tokens": 1, "total_tokens": 1 }
        }));
    });

    let mut config = AiConfig::default();
    config.enabled = true;
    config.embeddings.enabled = true;
    config.embeddings.backend = AiEmbeddingsBackend::Provider;
    config.provider.kind = AiProviderKind::AzureOpenAi;
    config.provider.url = Url::parse(&format!("{}/", server.base_url())).unwrap();
    config.provider.azure_deployment = Some("my-deployment".to_string());
    config.api_key = Some("test-key".to_string());

    // Cloud privacy mode.
    config.privacy.local_only = false;
    config.privacy.anonymize_identifiers = Some(false);
    config.privacy.redact_sensitive_strings = Some(false);
    config.privacy.redact_numeric_literals = Some(false);
    config.privacy.strip_or_redact_comments = Some(false);

    let client = embeddings_client_from_config(&config).expect("build embeddings client");
    let out = client
        .embed(
            &[format!("find refs in {abs_path_str}").to_string()],
            EmbeddingInputKind::Query,
            CancellationToken::new(),
        )
        .await
        .expect("embed");

    leaky.assert_hits(0);
    redacted.assert_hits(1);
    assert_eq!(out, vec![vec![1.0, 2.0, 3.0]]);
}

#[tokio::test]
async fn azure_openai_provider_embeddings_respects_embeddings_model_override_as_deployment() {
    let server = MockServer::start();

    let mock = server.mock(|when, then| {
        when.method(POST)
            .path("/openai/deployments/embed-deployment/embeddings")
            .query_param("api-version", "2024-02-01")
            .header("api-key", "test-key");
        then.status(200).json_body(json!({
            "object": "list",
            "data": [
                {
                    "object": "embedding",
                    "embedding": [1.0, 2.0, 3.0],
                    "index": 0
                }
            ],
            "model": "ignored",
            "usage": { "prompt_tokens": 1, "total_tokens": 1 }
        }));
    });

    let mut config = AiConfig::default();
    config.enabled = true;
    config.embeddings.enabled = true;
    config.embeddings.backend = AiEmbeddingsBackend::Provider;
    config.embeddings.model = Some("embed-deployment".to_string());
    config.provider.kind = AiProviderKind::AzureOpenAi;
    config.provider.url = Url::parse(&format!("{}/", server.base_url())).unwrap();
    config.provider.azure_deployment = Some("chat-deployment".to_string());
    config.api_key = Some("test-key".to_string());
    config.privacy.local_only = false;

    let client = embeddings_client_from_config(&config).expect("build embeddings client");
    let out = client
        .embed(
            &["hello".to_string()],
            EmbeddingInputKind::Query,
            CancellationToken::new(),
        )
        .await
        .expect("embed");

    mock.assert();
    assert_eq!(out, vec![vec![1.0, 2.0, 3.0]]);
}

fn json_string_contents(text: &str) -> String {
    // Provider embedding requests are JSON; paths containing backslashes will be escaped.
    let json = serde_json::to_string(text).expect("json string");
    json.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(&json)
        .to_string()
}
