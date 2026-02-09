#![cfg(feature = "embeddings")]

use httpmock::prelude::*;
use nova_ai::embeddings::{embeddings_client_from_config, EmbeddingInputKind};
use nova_ai::semantic_search_from_config;
use nova_config::{AiConfig, AiEmbeddingsBackend, AiProviderKind, ByteSize};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use url::Url;

#[tokio::test]
async fn provider_embeddings_are_cached_on_disk() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/v1/embeddings");
        then.status(200).json_body(json!({
            "data": [{
                "embedding": [0.25, 0.5, 0.75],
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
    config.embeddings.max_memory_bytes = ByteSize(1024 * 1024);
    config.provider.kind = AiProviderKind::OpenAiCompatible;
    config.provider.url = Url::parse(&server.base_url()).expect("base url");
    config.provider.model = "test-embedder".to_string();
    config.provider.timeout_ms = 2_000;

    let client = embeddings_client_from_config(&config).expect("embeddings client");

    let out1 = client
        .embed(
            &["hello world".to_string()],
            EmbeddingInputKind::Query,
            CancellationToken::new(),
        )
        .await
        .expect("embed");
    assert_eq!(out1, vec![vec![0.25, 0.5, 0.75]]);
    mock.assert_hits(1);

    drop(client);

    let client = embeddings_client_from_config(&config).expect("embeddings client");

    let out2 = client
        .embed(
            &["hello world".to_string()],
            EmbeddingInputKind::Query,
            CancellationToken::new(),
        )
        .await
        .expect("embed");
    assert_eq!(out2, vec![vec![0.25, 0.5, 0.75]]);

    // Second embed should hit the disk cache (no additional HTTP calls).
    mock.assert_hits(1);

    // Ensure we didn't persist raw text in the file layout.
    let cache_entries: Vec<_> = walkdir::WalkDir::new(&model_dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();
    assert!(
        cache_entries.iter().all(|name| !name.contains("hello")),
        "expected cache entry names to be hashed; got: {cache_entries:?}"
    );
}

#[test]
fn provider_semantic_search_query_embeddings_are_cached_on_disk() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/v1/embeddings");
        then.status(200).json_body(json!({
            "data": [{
                "embedding": [0.25, 0.5, 0.75],
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
    config.features.semantic_search = true;
    config.embeddings.enabled = true;
    config.embeddings.backend = AiEmbeddingsBackend::Provider;
    config.embeddings.model_dir = model_dir;
    config.provider.kind = AiProviderKind::OpenAiCompatible;
    config.provider.url = Url::parse(&server.base_url()).expect("base url");
    config.provider.model = "test-embedder".to_string();
    config.provider.timeout_ms = 2_000;

    let search = semantic_search_from_config(&config).expect("semantic search should build");
    assert!(search.search("hello world").is_empty());
    mock.assert_hits(1);

    drop(search);

    let search = semantic_search_from_config(&config).expect("semantic search should build");
    assert!(search.search("hello world").is_empty());
    // Second embed should hit the disk cache (no additional HTTP calls).
    mock.assert_hits(1);
}
