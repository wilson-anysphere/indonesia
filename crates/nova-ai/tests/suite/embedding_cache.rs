#![cfg(feature = "embeddings")]

use httpmock::prelude::*;
use nova_ai::embeddings::cache::EmbeddingVectorCache;
use nova_ai::embeddings::embeddings_client_from_config;
use nova_config::{AiConfig, AiEmbeddingsBackend, AiProviderKind, ByteSize};
use serde_json::json;
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;
use url::Url;

#[tokio::test]
async fn provider_embeddings_use_memory_cache_for_partial_hits() {
    let server = MockServer::start();

    let initial = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/embeddings")
            .json_body(json!({
                "model": "test-embedder",
                "input": ["alpha", "beta"],
            }));

        then.status(200).json_body(json!({
            "data": [
                { "embedding": [0.1, 0.0], "index": 0, "object": "embedding" },
                { "embedding": [0.2, 0.0], "index": 1, "object": "embedding" },
            ],
            "model": "test-embedder",
            "object": "list",
        }));
    });

    let only_miss = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/embeddings")
            .json_body(json!({
                "model": "test-embedder",
                "input": ["gamma"],
            }));

        then.status(200).json_body(json!({
            "data": [
                { "embedding": [0.3, 0.0], "index": 0, "object": "embedding" },
            ],
            "model": "test-embedder",
            "object": "list",
        }));
    });

    let mut config = AiConfig::default();
    config.enabled = true;
    config.embeddings.enabled = true;
    config.embeddings.backend = AiEmbeddingsBackend::Provider;
    config.embeddings.max_memory_bytes = ByteSize(1024 * 1024);
    // Disable disk cache for this test (we want to exercise the in-memory cache only).
    config.embeddings.model_dir = PathBuf::new();
    config.provider.kind = AiProviderKind::OpenAiCompatible;
    config.provider.url = Url::parse(&server.base_url()).expect("base url");
    config.provider.model = "test-embedder".to_string();
    config.provider.timeout_ms = 2_000;

    let client = embeddings_client_from_config(&config).expect("embeddings client");

    let out1 = client
        .embed(
            &["alpha".to_string(), "beta".to_string()],
            CancellationToken::new(),
        )
        .await
        .expect("embed");
    assert_eq!(out1, vec![vec![0.1, 0.0], vec![0.2, 0.0]]);

    let out2 = client
        .embed(
            &["alpha".to_string(), "gamma".to_string()],
            CancellationToken::new(),
        )
        .await
        .expect("embed");
    assert_eq!(out2, vec![vec![0.1, 0.0], vec![0.3, 0.0]]);

    initial.assert_hits(1);
    only_miss.assert_hits(1);
}

#[tokio::test]
async fn provider_embeddings_memory_cache_evicts_lru_entries() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/v1/embeddings");
        then.status(200).json_body(json!({
            "data": [{
                "embedding": [1.0, 0.0, 0.0, 0.0],
                "index": 0,
                "object": "embedding",
            }],
            "model": "test-embedder",
            "object": "list",
        }));
    });

    let dims = 4;
    let entry_bytes = EmbeddingVectorCache::estimate_entry_bytes(dims);

    let mut config = AiConfig::default();
    config.enabled = true;
    config.embeddings.enabled = true;
    config.embeddings.backend = AiEmbeddingsBackend::Provider;
    // Budget for exactly two entries.
    config.embeddings.max_memory_bytes = ByteSize((entry_bytes * 2) as u64);
    config.embeddings.model_dir = PathBuf::new();
    config.provider.kind = AiProviderKind::OpenAiCompatible;
    config.provider.url = Url::parse(&server.base_url()).expect("base url");
    config.provider.model = "test-embedder".to_string();
    config.provider.timeout_ms = 2_000;

    let client = embeddings_client_from_config(&config).expect("embeddings client");

    let _ = client
        .embed(&["a".to_string()], CancellationToken::new())
        .await
        .expect("embed a");
    let _ = client
        .embed(&["b".to_string()], CancellationToken::new())
        .await
        .expect("embed b");

    // Touch a so it becomes most-recently-used (b becomes LRU).
    let _ = client
        .embed(&["a".to_string()], CancellationToken::new())
        .await
        .expect("embed a (hit)");

    // Insert c, evicting b.
    let _ = client
        .embed(&["c".to_string()], CancellationToken::new())
        .await
        .expect("embed c");

    // a should still be cached.
    let _ = client
        .embed(&["a".to_string()], CancellationToken::new())
        .await
        .expect("embed a (still hit)");

    // b should have been evicted.
    let _ = client
        .embed(&["b".to_string()], CancellationToken::new())
        .await
        .expect("embed b (miss)");

    mock.assert_hits(4);
}
