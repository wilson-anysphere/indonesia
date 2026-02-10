#![cfg(feature = "embeddings")]

use httpmock::prelude::*;
use nova_ai::{Embedder, OllamaEmbedder};
use serde_json::json;
use std::time::Duration;
use url::Url;

#[test]
fn ollama_embedder_prefers_embed_endpoint() {
    let server = MockServer::start();

    let embed_mock = server.mock(|when, then| {
        when.method(POST).path("/api/embed").json_body(json!({
            "model": "test-model",
            "input": ["alpha", "beta"],
        }));
        then.status(200).json_body(json!({
            "embeddings": [
                [1.0_f32],
                [2.0_f32],
            ]
        }));
    });

    let embedder = OllamaEmbedder::new(
        Url::parse(&server.base_url()).expect("base url"),
        "test-model",
        Duration::from_secs(1),
        /*batch_size=*/ 16,
    )
    .expect("embedder builds");

    let out = embedder
        .embed_batch(&["alpha".to_string(), "beta".to_string()])
        .expect("embed batch succeeds");

    assert_eq!(out, vec![vec![1.0], vec![2.0]]);
    embed_mock.assert_hits(1);
}

#[test]
fn ollama_embedder_falls_back_to_legacy_endpoint_and_caches_probe() {
    let server = MockServer::start();

    let missing_batch = server.mock(|when, then| {
        when.method(POST).path("/api/embed");
        then.status(404);
    });

    let alpha_mock = server.mock(|when, then| {
        when.method(POST).path("/api/embeddings").json_body(json!({
            "model": "test-model",
            "prompt": "alpha",
        }));
        then.status(200).json_body(json!({
            "embedding": [1.0_f32],
        }));
    });

    let beta_mock = server.mock(|when, then| {
        when.method(POST).path("/api/embeddings").json_body(json!({
            "model": "test-model",
            "prompt": "beta",
        }));
        then.status(200).json_body(json!({
            "embedding": [2.0_f32],
        }));
    });

    let embedder = OllamaEmbedder::new(
        Url::parse(&server.base_url()).expect("base url"),
        "test-model",
        Duration::from_secs(1),
        /*batch_size=*/ 16,
    )
    .expect("embedder builds");

    assert_eq!(
        embedder.embed("alpha").expect("alpha embed"),
        vec![1.0]
    );
    assert_eq!(embedder.embed("beta").expect("beta embed"), vec![2.0]);

    // Ensure the embedder only probes the batch endpoint once.
    missing_batch.assert_hits(1);
    alpha_mock.assert_hits(1);
    beta_mock.assert_hits(1);
}

#[test]
fn ollama_embedder_falls_back_to_legacy_endpoint_when_embed_errors() {
    let server = MockServer::start();

    let embed_mock = server.mock(|when, then| {
        when.method(POST).path("/api/embed").json_body(json!({
            "model": "test-model",
            "input": ["alpha", "beta"],
        }));
        then.status(500).json_body(json!({"error": "boom"}));
    });

    let alpha_mock = server.mock(|when, then| {
        when.method(POST).path("/api/embeddings").json_body(json!({
            "model": "test-model",
            "prompt": "alpha",
        }));
        then.status(200).json_body(json!({
            "embedding": [1.0_f32],
        }));
    });

    let beta_mock = server.mock(|when, then| {
        when.method(POST).path("/api/embeddings").json_body(json!({
            "model": "test-model",
            "prompt": "beta",
        }));
        then.status(200).json_body(json!({
            "embedding": [2.0_f32],
        }));
    });

    let embedder = OllamaEmbedder::new(
        Url::parse(&server.base_url()).expect("base url"),
        "test-model",
        Duration::from_secs(1),
        /*batch_size=*/ 16,
    )
    .expect("embedder builds");

    let out = embedder
        .embed_batch(&["alpha".to_string(), "beta".to_string()])
        .expect("embed batch succeeds");
    assert_eq!(out, vec![vec![1.0_f32], vec![2.0_f32]]);

    embed_mock.assert_hits(1);
    alpha_mock.assert_hits(1);
    beta_mock.assert_hits(1);
}

#[test]
fn ollama_embedder_supports_base_url_with_api_suffix() {
    let server = MockServer::start();

    let embed_mock = server.mock(|when, then| {
        when.method(POST).path("/api/embed").json_body(json!({
            "model": "test-model",
            "input": ["alpha"],
        }));
        then.status(200).json_body(json!({
            "embeddings": [[1.0_f32]]
        }));
    });

    let embedder = OllamaEmbedder::new(
        Url::parse(&format!("{}/api", server.base_url())).expect("base url"),
        "test-model",
        Duration::from_secs(1),
        /*batch_size=*/ 16,
    )
    .expect("embedder builds");

    assert_eq!(embedder.embed("alpha").expect("embed alpha"), vec![1.0_f32]);
    embed_mock.assert_hits(1);
}
