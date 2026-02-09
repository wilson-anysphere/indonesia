#![cfg(feature = "embeddings")]

use httpmock::prelude::*;
use httpmock::Mock;
use nova_ai::{
    semantic_search_from_config, AiError, Embedder, OpenAiCompatibleEmbedder, VirtualWorkspace,
};
use nova_config::{
    AiConfig, AiEmbeddingsBackend, AiEmbeddingsConfig, AiFeaturesConfig, AiProviderKind,
};
use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;
use url::Url;

fn provider_config(server: &MockServer, base_with_v1: bool, api_key: Option<&str>) -> AiConfig {
    let mut cfg = AiConfig::default();
    cfg.enabled = true;
    cfg.features = AiFeaturesConfig {
        semantic_search: true,
        ..AiFeaturesConfig::default()
    };
    cfg.embeddings = AiEmbeddingsConfig {
        enabled: true,
        backend: AiEmbeddingsBackend::Provider,
        ..AiEmbeddingsConfig::default()
    };
    cfg.provider.kind = AiProviderKind::OpenAiCompatible;
    cfg.provider.model = "test-embedding-model".to_string();
    cfg.provider.timeout_ms = Duration::from_secs(2).as_millis() as u64;

    let base = if base_with_v1 {
        format!("{}/v1", server.base_url())
    } else {
        server.base_url()
    };
    cfg.provider.url = Url::parse(&base).expect("valid base url");
    cfg.api_key = api_key.map(str::to_string);
    cfg
}

fn workspace() -> VirtualWorkspace {
    VirtualWorkspace::new([
        ("src/hello.txt".to_string(), "hello world".to_string()),
        ("src/goodbye.txt".to_string(), "goodbye".to_string()),
    ])
}

fn mock_embedding<'a>(
    server: &'a MockServer,
    needle: &str,
    embedding: [f32; 2],
    auth: Option<&str>,
) -> Mock<'a> {
    server.mock(move |when, then| {
        if let Some(auth) = auth {
            when.method(POST)
                .path("/v1/embeddings")
                .body_contains("\"input\":[")
                .body_contains(needle)
                .header("authorization", auth);
        } else {
            when.method(POST)
                .path("/v1/embeddings")
                .body_contains("\"input\":[")
                .body_contains(needle);
        }
        then.status(200).json_body(json!({
            "object": "list",
            "model": "test-embedding-model",
            "data": [{
                "object": "embedding",
                "index": 0,
                "embedding": embedding,
            }],
            "usage": { "prompt_tokens": 0, "total_tokens": 0 }
        }));
    })
}

#[test]
fn provider_embeddings_base_url_without_v1_sends_authorization_and_ranks_results() {
    let server = MockServer::start();
    let auth = "Bearer test-key";

    let hello = mock_embedding(&server, "hello world", [1.0, 0.0], Some(auth));
    let goodbye = mock_embedding(&server, "goodbye", [0.0, 1.0], Some(auth));

    let cfg = provider_config(&server, /*base_with_v1=*/ false, Some("test-key"));
    let db = workspace();

    let mut search = semantic_search_from_config(&cfg).expect("build semantic search");
    search.index_project(&db);

    let results1 = search.search("hello world");
    let results2 = search.search("hello world");

    assert!(!results1.is_empty());
    assert_eq!(
        results1, results2,
        "expected stable ordering across searches"
    );
    assert_eq!(results1[0].path, PathBuf::from("src/hello.txt"));

    // One embed call for indexing + one for searching (the second search reuses the cached query embedding).
    hello.assert_hits(2);
    goodbye.assert_hits(1);
}

#[test]
fn provider_embeddings_base_url_with_v1_is_normalized() {
    let server = MockServer::start();

    let hello = mock_embedding(&server, "hello world", [1.0, 0.0], None);
    let goodbye = mock_embedding(&server, "goodbye", [0.0, 1.0], None);

    let cfg = provider_config(&server, /*base_with_v1=*/ true, None);
    let db = workspace();

    let mut search = semantic_search_from_config(&cfg).expect("build semantic search");
    search.index_project(&db);

    let results = search.search("hello world");
    assert!(!results.is_empty());
    assert_eq!(results[0].path, PathBuf::from("src/hello.txt"));

    hello.assert_hits(2);
    goodbye.assert_hits(1);
}

#[test]
fn provider_embedder_parses_batch_embeddings_in_index_order() {
    let server = MockServer::start();

    let mock = server.mock(|when, then| {
        when.method(POST).path("/v1/embeddings").json_body(json!({
            "model": "test-embedding-model",
            "input": ["first", "second"],
        }));
        then.status(200).json_body(json!({
            "data": [
                { "index": 1, "embedding": [0.0, 1.0] },
                { "index": 0, "embedding": [1.0, 0.0] }
            ]
        }));
    });

    let embedder = OpenAiCompatibleEmbedder::new(
        Url::parse(&server.base_url()).expect("server url"),
        "test-embedding-model",
        Duration::from_secs(2),
        None,
        /*batch_size=*/ 32,
    )
    .expect("embedder builds");

    let out = embedder
        .embed_batch(&["first".to_string(), "second".to_string()])
        .expect("embed_batch");
    assert_eq!(out, vec![vec![1.0, 0.0], vec![0.0, 1.0]]);
    mock.assert_hits(1);
}

#[test]
fn provider_embedder_rejects_inconsistent_embedding_dimensions() {
    let server = MockServer::start();

    let _mock = server.mock(|when, then| {
        when.method(POST).path("/v1/embeddings");
        then.status(200).json_body(json!({
            "data": [
                { "index": 0, "embedding": [1.0, 0.0] },
                { "index": 1, "embedding": [0.0, 1.0, 2.0] }
            ]
        }));
    });

    let embedder = OpenAiCompatibleEmbedder::new(
        Url::parse(&server.base_url()).expect("server url"),
        "test-embedding-model",
        Duration::from_secs(2),
        None,
        /*batch_size=*/ 32,
    )
    .expect("embedder builds");

    let err = embedder
        .embed_batch(&["first".to_string(), "second".to_string()])
        .expect_err("expected dimension mismatch error");

    assert!(
        matches!(err, AiError::UnexpectedResponse(_)),
        "unexpected error: {err:?}"
    );
}
