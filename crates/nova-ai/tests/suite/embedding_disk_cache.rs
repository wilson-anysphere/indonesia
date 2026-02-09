#![cfg(feature = "embeddings")]

use httpmock::prelude::*;
use nova_ai::embeddings::{embeddings_client_from_config, EmbeddingInputKind};
use nova_ai::{semantic_search_from_config, AiError, VirtualWorkspace};
use nova_config::{AiConfig, AiEmbeddingsBackend, AiProviderKind, ByteSize};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use url::Url;

#[tokio::test(flavor = "current_thread")]
async fn provider_embeddings_are_cached_on_disk() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/embeddings")
            .json_body(json!({
                "model": "test-embedder",
                "input": ["hello world"],
            }));
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

#[test]
fn provider_embeddings_disk_cache_requires_model_dir_to_be_a_directory() {
    let tmp_file = tempfile::NamedTempFile::new().expect("temp file");
    let model_dir = tmp_file
        .path()
        .canonicalize()
        .unwrap_or_else(|_| tmp_file.path().to_path_buf());

    let mut config = AiConfig::default();
    config.enabled = true;
    config.embeddings.enabled = true;
    config.embeddings.backend = AiEmbeddingsBackend::Provider;
    config.embeddings.model_dir = model_dir.clone();
    config.provider.kind = AiProviderKind::OpenAiCompatible;
    config.provider.url = Url::parse("http://localhost:1234/v1").expect("url");
    config.provider.model = "test-embedder".to_string();
    config.provider.timeout_ms = 2_000;

    let err = match embeddings_client_from_config(&config) {
        Ok(_) => panic!("expected invalid config for model_dir {}", model_dir.display()),
        Err(err) => err,
    };
    assert!(
        matches!(&err, AiError::InvalidConfig(_)),
        "expected InvalidConfig, got: {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("ai.embeddings.model_dir"),
        "unexpected error message: {msg}"
    );
}

#[test]
fn provider_semantic_search_index_embeddings_are_cached_on_disk() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/v1/embeddings");
        then.status(200).json_body(json!({
            "data": [
                { "index": 0, "embedding": [0.25, 0.5, 0.75] },
                { "index": 1, "embedding": [0.25, 0.5, 0.75] },
                { "index": 2, "embedding": [0.25, 0.5, 0.75] },
            ],
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

    let db = VirtualWorkspace::new([(
        "src/Hello.java".to_string(),
        r#"
            package com.example;

            public class Hello {
                public String helloWorld() {
                    return "hello world";
                }

                public String goodbye() {
                    return "goodbye";
                }
            }
        "#
        .to_string(),
    )]);

    let mut search = semantic_search_from_config(&config).expect("semantic search should build");
    search.index_project(&db);
    let first_hits = mock.hits();
    assert_eq!(
        first_hits, 1,
        "expected indexing to batch embeddings into a single provider request"
    );

    drop(search);

    let mut search = semantic_search_from_config(&config).expect("semantic search should build");
    search.index_project(&db);

    assert_eq!(
        mock.hits(),
        first_hits,
        "expected second indexing run to use disk cache"
    );
}
