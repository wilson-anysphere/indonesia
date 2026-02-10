#![cfg(feature = "embeddings")]

use httpmock::prelude::*;
use nova_ai::embeddings::{embeddings_client_from_config, EmbeddingInputKind};
use nova_config::{AiConfig, AiEmbeddingsBackend, AiProviderKind};
use serde_json::json;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use url::Url;

#[tokio::test(flavor = "current_thread")]
async fn provider_embeddings_respect_configured_batch_size() {
    let server = MockServer::start();

    let mock_ab = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/embeddings")
            .json_body(json!({
                "model": "test-embed-model",
                "input": ["a", "b"],
            }));
        then.status(200).json_body(json!({
            "data": [
                {
                    "embedding": [1.0],
                    "index": 0,
                    "object": "embedding",
                },
                {
                    "embedding": [2.0],
                    "index": 1,
                    "object": "embedding",
                }
            ],
            "object": "list",
        }))
        .delay(Duration::from_millis(100));
    });

    let dir = tempfile::tempdir().expect("tempdir");
    let model_dir = dir.path().join("models").join("embeddings");

    let mut config = AiConfig::default();
    config.enabled = true;
    config.embeddings.enabled = true;
    config.embeddings.backend = AiEmbeddingsBackend::Provider;
    config.embeddings.batch_size = 2;
    config.embeddings.model_dir = model_dir;
    config.provider.kind = AiProviderKind::OpenAiCompatible;
    config.provider.url = Url::parse(&server.base_url()).expect("base url");
    config.provider.model = "test-embed-model".to_string();
    config.provider.timeout_ms = 2_000;

    let client = embeddings_client_from_config(&config).expect("embeddings client");
    let cancel = CancellationToken::new();
    let handle = tokio::spawn(async move {
        client
            .embed(
                &["a".to_string(), "b".to_string(), "c".to_string()],
                EmbeddingInputKind::Query,
                cancel,
            )
            .await
    });

    // Enforce request ordering: only register the second batch response after we observe the first
    // one has arrived. If batching happens out of order, the task will fail (no matching mock).
    tokio::time::timeout(Duration::from_secs(1), async {
        while mock_ab.hits() < 1 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("expected first batch request to be sent");

    let mock_c = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/embeddings")
            .json_body(json!({
                "model": "test-embed-model",
                "input": ["c"],
            }));
        then.status(200).json_body(json!({
            "data": [
                {
                    "embedding": [3.0],
                    "index": 0,
                    "object": "embedding",
                }
            ],
            "object": "list",
        }));
    });

    let out = handle.await.expect("embed task").expect("embed");

    mock_ab.assert_hits(1);
    mock_c.assert_hits(1);

    assert_eq!(out, vec![vec![1.0], vec![2.0], vec![3.0]]);
}
