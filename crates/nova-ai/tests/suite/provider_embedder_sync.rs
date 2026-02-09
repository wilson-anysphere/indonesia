#![cfg(feature = "embeddings")]

use httpmock::prelude::*;
use nova_ai::{EmbeddingSemanticSearch, OpenAiCompatibleEmbedder, SemanticSearch};
use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;
use url::Url;

#[test]
fn provider_embedder_can_be_used_from_sync_context_without_tokio_runtime() {
    // This test is explicitly *not* a `#[tokio::test]` because semantic search is synchronous.
    // If the embedder implementation incorrectly depends on `tokio::runtime::Handle::current()`,
    // this assertion (and the calls below) will fail.
    assert!(
        tokio::runtime::Handle::try_current().is_err(),
        "expected no tokio runtime in plain #[test]"
    );

    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/v1/embeddings");
        then.status(200).json_body(json!({
            "data": [{"index": 0, "embedding": [1.0, 0.0, 0.0]}],
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

    let mut search = EmbeddingSemanticSearch::new(embedder);
    let path = PathBuf::from("src/example.txt");
    search.index_file(path.clone(), "hello world".to_string());

    let results = search.search("hello world");
    assert!(!results.is_empty(), "expected non-empty results");
    assert_eq!(results[0].path, path);

    // One embed call for indexing + one for searching.
    mock.assert_hits(2);
}

#[test]
fn provider_embedder_batches_embedding_requests_when_indexing_multiple_docs() {
    let server = MockServer::start();
    let index_mock = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/embeddings")
            .body_contains("goodbye");
        then.status(200).json_body(json!({
            "data": [
                { "index": 0, "embedding": [1.0, 0.0, 0.0] },
                { "index": 1, "embedding": [1.0, 0.0, 0.0] },
                { "index": 2, "embedding": [1.0, 0.0, 0.0] }
            ],
        }));
    });

    let query_mock = server.mock(|when, then| {
        when.method(POST).path("/v1/embeddings").json_body(json!({
            "model": "test-embedding-model",
            "input": ["hello world"],
        }));
        then.status(200).json_body(json!({
            "data": [{ "index": 0, "embedding": [1.0, 0.0, 0.0] }]
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

    let mut search = EmbeddingSemanticSearch::new(embedder);
    let path = PathBuf::from("src/Hello.java");

    // This file produces multiple extracted docs (type + methods), which should trigger a
    // single `embed_batch` call.
    search.index_file(
        path.clone(),
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
    );

    index_mock.assert_hits(1);
    query_mock.assert_hits(0);

    let results = search.search("hello world");
    assert!(!results.is_empty(), "expected non-empty results");
    assert_eq!(results[0].path, path);

    // One batched embed call for indexing + one for searching.
    index_mock.assert_hits(1);
    query_mock.assert_hits(1);
}
