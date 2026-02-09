#![cfg(feature = "embeddings")]

use httpmock::prelude::*;
use nova_ai::embeddings::{embeddings_client_from_config, EmbeddingInputKind};
use nova_config::{AiConfig, AiEmbeddingsBackend, AiProviderKind};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use url::Url;

#[tokio::test]
async fn ollama_provider_embeddings_prefers_batch_embed_endpoint() {
    let server = MockServer::start();

    let expected_body = json!({
        "model": "test-embed-model",
        "input": ["alpha", "beta"],
    });
    let mock = server.mock(|when, then| {
        when.method(POST).path("/api/embed").json_body(expected_body);
        then.status(200).json_body(json!({
            "model": "ignored",
            "embeddings": [
                [1.0, 0.0, 0.0],
                [0.0, 1.0, 0.0]
            ]
        }));
    });

    let mut config = AiConfig::default();
    config.enabled = true;
    config.embeddings.enabled = true;
    config.embeddings.backend = AiEmbeddingsBackend::Provider;
    config.embeddings.model = Some("test-embed-model".to_string());
    config.provider.kind = AiProviderKind::Ollama;
    config.provider.url = Url::parse(&server.base_url()).unwrap();
    config.provider.model = "ignored-provider-model".to_string();

    let client = embeddings_client_from_config(&config).expect("build embeddings client");
    let out = client
        .embed(
            &["alpha".to_string(), "beta".to_string()],
            EmbeddingInputKind::Query,
            CancellationToken::new(),
        )
        .await
        .expect("embed");

    mock.assert_hits(1);
    assert_eq!(
        out,
        vec![vec![1.0, 0.0, 0.0], vec![0.0, 1.0, 0.0]],
        "expected embeddings in the same order as inputs"
    );
}

#[tokio::test]
async fn ollama_provider_embeddings_falls_back_to_legacy_embeddings_endpoint() {
    let server = MockServer::start();

    let embed_body = json!({
        "model": "test-provider-model",
        "input": ["first", "second"],
    });
    let embed_mock = server.mock(|when, then| {
        when.method(POST).path("/api/embed").json_body(embed_body);
        then.status(404);
    });

    let first_mock = server.mock(|when, then| {
        when.method(POST)
            .path("/api/embeddings")
            .json_body(json!({
                "model": "test-provider-model",
                "prompt": "first",
            }));
        then.status(200).json_body(json!({
            "embedding": [9.0, 0.0]
        }));
    });

    let second_mock = server.mock(|when, then| {
        when.method(POST)
            .path("/api/embeddings")
            .json_body(json!({
                "model": "test-provider-model",
                "prompt": "second",
            }));
        then.status(200).json_body(json!({
            "embedding": [0.0, 9.0]
        }));
    });

    let mut config = AiConfig::default();
    config.enabled = true;
    config.embeddings.enabled = true;
    config.embeddings.backend = AiEmbeddingsBackend::Provider;
    config.provider.kind = AiProviderKind::Ollama;
    config.provider.url = Url::parse(&server.base_url()).unwrap();
    config.provider.model = "test-provider-model".to_string();

    let client = embeddings_client_from_config(&config).expect("build embeddings client");
    let out = client
        .embed(
            &["first".to_string(), "second".to_string()],
            EmbeddingInputKind::Query,
            CancellationToken::new(),
        )
        .await
        .expect("embed");

    embed_mock.assert_hits(1);
    first_mock.assert_hits(1);
    second_mock.assert_hits(1);
    assert_eq!(
        out,
        vec![vec![9.0, 0.0], vec![0.0, 9.0]],
        "expected embeddings in the same order as inputs"
    );
}
