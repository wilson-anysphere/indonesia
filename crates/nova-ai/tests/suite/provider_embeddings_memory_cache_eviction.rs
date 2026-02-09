#![cfg(feature = "embeddings")]

use httpmock::prelude::*;
use nova_ai::embeddings::{embeddings_client_from_config, EmbeddingInputKind};
use nova_config::{AiConfig, AiEmbeddingsBackend, AiProviderKind, ByteSize};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use url::Url;

#[tokio::test(flavor = "current_thread")]
async fn provider_embeddings_memory_cache_is_lru_evicted_by_byte_budget() {
    const DIMS: usize = 1024;
    const EMBEDDING_BYTES: u64 = (DIMS as u64) * 4;

    let embedding: Vec<f32> = (0..DIMS).map(|i| i as f32).collect();

    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/v1/embeddings");
        then.status(200).json_body(json!({
            "data": [{
                "embedding": embedding,
                "index": 0,
                "object": "embedding",
            }],
            "model": "test-embedder",
            "object": "list",
        }));
    });

    let dir = tempfile::tempdir().expect("tempdir");
    let model_dir = dir.path().join("models").join("embeddings");

    let mut config = AiConfig::default();
    config.enabled = true;
    config.embeddings.enabled = true;
    config.embeddings.backend = AiEmbeddingsBackend::Provider;
    config.embeddings.model_dir = model_dir.clone();
    config.embeddings.max_memory_bytes = ByteSize(EMBEDDING_BYTES); // Fits exactly 1 embedding.
    config.provider.kind = AiProviderKind::OpenAiCompatible;
    config.provider.url = Url::parse(&server.base_url()).expect("base url");
    config.provider.model = "test-embedder".to_string();
    config.provider.timeout_ms = 2_000;

    let client = embeddings_client_from_config(&config).expect("embeddings client");

    let out1 = client
        .embed(
            &["a".to_string()],
            EmbeddingInputKind::Query,
            CancellationToken::new(),
        )
        .await
        .expect("embed a");
    assert_eq!(out1.len(), 1);
    assert_eq!(out1[0].len(), DIMS);
    mock.assert_hits(1);

    let out2 = client
        .embed(
            &["b".to_string()],
            EmbeddingInputKind::Query,
            CancellationToken::new(),
        )
        .await
        .expect("embed b");
    assert_eq!(out2.len(), 1);
    assert_eq!(out2[0].len(), DIMS);
    mock.assert_hits(2);

    // Remove any disk-cache entries so the next request can only be satisfied from memory or the
    // network.
    std::fs::remove_dir_all(&model_dir).expect("remove model_dir");

    // `b` should still be cached in memory (no extra network hit).
    let out3 = client
        .embed(
            &["b".to_string()],
            EmbeddingInputKind::Query,
            CancellationToken::new(),
        )
        .await
        .expect("embed b (memory hit)");
    assert_eq!(out3.len(), 1);
    assert_eq!(out3[0].len(), DIMS);
    mock.assert_hits(2);

    let out4 = client
        .embed(
            &["a".to_string()],
            EmbeddingInputKind::Query,
            CancellationToken::new(),
        )
        .await
        .expect("embed a again");
    assert_eq!(out4.len(), 1);
    assert_eq!(out4[0].len(), DIMS);

    // If the in-memory cache evicted `a` after caching `b`, this must be a third HTTP hit (with
    // disk-cache entries removed).
    mock.assert_hits(3);
}
