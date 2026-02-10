#![cfg(feature = "embeddings")]

use httpmock::prelude::*;
use nova_ai::{
    semantic_search_from_config, Embedder, OpenAiCompatibleEmbedder, VirtualWorkspace,
};
use nova_config::{AiEmbeddingsBackend, AiProviderKind};
use serde_json::json;
use std::time::Duration;
use url::Url;

#[test]
fn openai_compatible_embed_batch_chunks_by_configured_batch_size() {
    let server = MockServer::start();

    let first = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/embeddings")
            .json_body(json!({
                "model": "test-model",
                "input": ["a", "b"],
            }));
        then.status(200).json_body(json!({
            "data": [
                { "index": 0, "embedding": [1.0] },
                { "index": 1, "embedding": [2.0] },
            ]
        }));
    });

    let second = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/embeddings")
            .json_body(json!({
                "model": "test-model",
                "input": ["c"],
            }));
        then.status(200).json_body(json!({
            "data": [
                { "index": 0, "embedding": [3.0] },
            ]
        }));
    });

    let embedder = OpenAiCompatibleEmbedder::new(
        Url::parse(&server.base_url()).expect("base url"),
        "test-model",
        Duration::from_secs(1),
        None,
        /*batch_size=*/ 2,
    )
    .expect("build embedder");

    let inputs = vec!["a".to_string(), "b".to_string(), "c".to_string()];
    let embeddings = embedder
        .embed_batch(&inputs)
        .expect("batch embedding succeeds");
    assert_eq!(embeddings, vec![vec![1.0], vec![2.0], vec![3.0]]);

    first.assert_hits(1);
    second.assert_hits(1);
}

#[test]
fn provider_embedder_chunks_openai_compatible_batches_when_configured() {
    let server = MockServer::start();

    // If the provider embedder does not chunk, all extracted docs will be sent in one request,
    // which contains both method markers.
    let hello_chunk = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/embeddings")
            .body_contains("\"input\":[")
            .body_contains("name: helloWorld");
        then.status(200).json_body(json!({
            "data": [
                { "index": 0, "embedding": [1.0, 0.0, 0.0] },
                { "index": 1, "embedding": [1.0, 0.0, 0.0] },
            ]
        }));
    });

    let goodbye_chunk = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/embeddings")
            .body_contains("\"input\":[")
            .body_contains("name: goodbye");
        then.status(200).json_body(json!({
            "data": [
                { "index": 0, "embedding": [1.0, 0.0, 0.0] },
            ]
        }));
    });

    // Per-doc fallback uses the string-shaped OpenAI embedding request. If we ever hit this,
    // batching/chunking is not working correctly.
    let fallback = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/embeddings")
            .body_contains("\"input\":\"")
            .body_contains("path: src/Hello.java");
        then.status(413);
    });

    let query = server.mock(|when, then| {
        when.method(POST).path("/v1/embeddings").json_body(json!({
            "model": "text-embedding-3-small",
            "input": ["hello world"],
        }));
        then.status(200).json_body(json!({
            "data": [{ "embedding": [1.0, 0.0, 0.0] }]
        }));
    });

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

    let mut cfg = nova_config::AiConfig::default();
    cfg.enabled = true;
    cfg.features.semantic_search = true;
    cfg.embeddings.enabled = true;
    cfg.embeddings.backend = AiEmbeddingsBackend::Provider;
    cfg.embeddings.model = Some("text-embedding-3-small".to_string());
    cfg.embeddings.batch_size = 2;
    cfg.provider.kind = AiProviderKind::OpenAiCompatible;
    cfg.provider.url = Url::parse(&format!("{}/v1", server.base_url())).unwrap();

    let mut search = semantic_search_from_config(&cfg).expect("semantic search should build");
    search.index_project(&db);

    let results = search.search("hello world");
    assert!(!results.is_empty());

    hello_chunk.assert_hits(1);
    goodbye_chunk.assert_hits(1);
    query.assert_hits(1);
    fallback.assert_hits(0);
}

#[test]
fn provider_embedder_respects_openai_embedding_indices() {
    let server = MockServer::start();

    // Indexing a Java file yields multiple extracted docs (type + method). Return the embeddings
    // out-of-order to ensure the parser respects `index` fields rather than response ordering.
    let index_mock = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/embeddings")
            .body_contains("\"input\":[")
            .body_contains("kind: type")
            .body_contains("kind: method");
        then.status(200).json_body(json!({
            "data": [
                { "index": 1, "embedding": [0.0, 1.0] },
                { "index": 0, "embedding": [1.0, 0.0] }
            ]
        }));
    });

    let query_mock = server.mock(|when, then| {
        when.method(POST).path("/v1/embeddings").json_body(json!({
            "model": "text-embedding-3-small",
            "input": ["QUERY"],
        }));
        then.status(200).json_body(json!({
            "data": [{ "index": 0, "embedding": [0.0, 1.0] }]
        }));
    });

    let db = VirtualWorkspace::new([(
        "src/Hello.java".to_string(),
        r#"
            package com.example;

            public class Hello {
                public String helloWorld() {
                    return "hello world";
                }
            }
        "#
        .to_string(),
    )]);

    let mut cfg = nova_config::AiConfig::default();
    cfg.enabled = true;
    cfg.features.semantic_search = true;
    cfg.embeddings.enabled = true;
    cfg.embeddings.backend = AiEmbeddingsBackend::Provider;
    cfg.embeddings.model = Some("text-embedding-3-small".to_string());
    cfg.embeddings.batch_size = 16;
    cfg.provider.kind = AiProviderKind::OpenAiCompatible;
    cfg.provider.url = Url::parse(&format!("{}/v1", server.base_url())).unwrap();

    let mut search = semantic_search_from_config(&cfg).expect("semantic search should build");
    search.index_project(&db);

    let results = search.search("QUERY");
    assert!(!results.is_empty());
    assert_eq!(results[0].kind, "method");

    index_mock.assert_hits(1);
    query_mock.assert_hits(1);
}
