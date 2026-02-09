#![cfg(feature = "embeddings")]

use std::path::PathBuf;

use httpmock::prelude::*;
use nova_ai::{semantic_search_from_config, VirtualWorkspace};
use nova_config::{AiConfig, AiEmbeddingsBackend, AiFeaturesConfig, AiProviderKind};
use serde_json::json;
use url::Url;

fn config_for_server(server: &MockServer) -> AiConfig {
    let mut cfg = AiConfig::default();
    cfg.enabled = true;
    cfg.features = AiFeaturesConfig {
        semantic_search: true,
        ..AiFeaturesConfig::default()
    };

    cfg.provider.kind = AiProviderKind::Ollama;
    cfg.provider.url = Url::parse(&server.base_url()).expect("valid url");
    cfg.provider.model = "chat-model".to_string();

    cfg.embeddings.enabled = true;
    cfg.embeddings.backend = AiEmbeddingsBackend::Provider;
    cfg.embeddings.batch_size = 16;
    cfg.embeddings.model = Some("embed-model".to_string());

    cfg
}

fn workspace() -> VirtualWorkspace {
    VirtualWorkspace::new([
        (
            "src/Hello.java".to_string(),
            r#"
                public class Hello {
                    public String helloWorld() {
                        return "hello world";
                    }
                }
            "#
            .to_string(),
        ),
        (
            "src/Other.java".to_string(),
            r#"
                public class Other {
                    public String goodbye() {
                        return "goodbye";
                    }
                }
            "#
            .to_string(),
        ),
    ])
}

#[test]
fn provider_embeddings_ollama_uses_batch_embed_endpoint_with_model_override() {
    let server = MockServer::start();

    let hello_mock = server.mock(|when, then| {
        when.method(POST)
            .path("/api/embed")
            .body_contains("\"model\":\"embed-model\"")
            .body_contains("src/Hello.java");
        then.status(200)
            .json_body(json!({ "embeddings": [[1.0_f32, 0.0_f32], [1.0_f32, 0.0_f32]] }));
    });

    let other_mock = server.mock(|when, then| {
        when.method(POST)
            .path("/api/embed")
            .body_contains("\"model\":\"embed-model\"")
            .body_contains("src/Other.java");
        then.status(200)
            .json_body(json!({ "embeddings": [[0.0_f32, 1.0_f32], [0.0_f32, 1.0_f32]] }));
    });

    let query_mock = server.mock(|when, then| {
        when.method(POST)
            .path("/api/embed")
            .body_contains("\"model\":\"embed-model\"")
            .body_contains("\"input\":[\"hello world\"]");
        then.status(200)
            .json_body(json!({ "embeddings": [[1.0_f32, 0.0_f32]] }));
    });

    let embeddings_endpoint_mock = server.mock(|when, then| {
        when.method(POST).path("/api/embeddings");
        then.status(500);
    });

    let cfg = config_for_server(&server);
    let mut search = semantic_search_from_config(&cfg).expect("semantic search should build");
    search.index_project(&workspace());

    let results = search.search("hello world");
    assert!(!results.is_empty());
    assert_eq!(results[0].path, PathBuf::from("src/Hello.java"));
    assert_eq!(results[0].kind, "method");

    hello_mock.assert_hits(1);
    other_mock.assert_hits(1);
    query_mock.assert_hits(1);
    embeddings_endpoint_mock.assert_hits(0);
}

#[test]
fn provider_embeddings_ollama_supports_base_url_with_api_suffix() {
    let server = MockServer::start();

    let hello_mock = server.mock(|when, then| {
        when.method(POST)
            .path("/api/embed")
            .body_contains("\"model\":\"embed-model\"")
            .body_contains("src/Hello.java");
        then.status(200)
            .json_body(json!({ "embeddings": [[1.0_f32, 0.0_f32], [1.0_f32, 0.0_f32]] }));
    });

    let other_mock = server.mock(|when, then| {
        when.method(POST)
            .path("/api/embed")
            .body_contains("\"model\":\"embed-model\"")
            .body_contains("src/Other.java");
        then.status(200)
            .json_body(json!({ "embeddings": [[0.0_f32, 1.0_f32], [0.0_f32, 1.0_f32]] }));
    });

    let query_mock = server.mock(|when, then| {
        when.method(POST)
            .path("/api/embed")
            .body_contains("\"model\":\"embed-model\"")
            .body_contains("\"input\":[\"hello world\"]");
        then.status(200)
            .json_body(json!({ "embeddings": [[1.0_f32, 0.0_f32]] }));
    });

    let embeddings_endpoint_mock = server.mock(|when, then| {
        when.method(POST).path("/api/embeddings");
        then.status(500);
    });

    let mut cfg = config_for_server(&server);
    cfg.provider.url = Url::parse(&format!("{}/api", server.base_url())).expect("valid url");

    let mut search = semantic_search_from_config(&cfg).expect("semantic search should build");
    search.index_project(&workspace());

    let results = search.search("hello world");
    assert!(!results.is_empty());
    assert_eq!(results[0].path, PathBuf::from("src/Hello.java"));

    hello_mock.assert_hits(1);
    other_mock.assert_hits(1);
    query_mock.assert_hits(1);
    embeddings_endpoint_mock.assert_hits(0);
}

#[test]
fn provider_embeddings_ollama_falls_back_to_single_endpoint_when_batch_is_missing() {
    let server = MockServer::start();

    // Older Ollama versions don't provide `/api/embed`. The embedder should fall back to the
    // per-input `/api/embeddings` endpoint.
    let missing_batch = server.mock(|when, then| {
        when.method(POST).path("/api/embed");
        then.status(404);
    });

    let hello_mock = server.mock(|when, then| {
        when.method(POST)
            .path("/api/embeddings")
            .body_contains("\"model\":\"embed-model\"")
            .body_contains("src/Hello.java");
        then.status(200)
            .json_body(json!({ "embedding": [1.0_f32, 0.0_f32] }));
    });

    let other_mock = server.mock(|when, then| {
        when.method(POST)
            .path("/api/embeddings")
            .body_contains("\"model\":\"embed-model\"")
            .body_contains("src/Other.java");
        then.status(200)
            .json_body(json!({ "embedding": [0.0_f32, 1.0_f32] }));
    });

    let query_mock = server.mock(|when, then| {
        when.method(POST)
            .path("/api/embeddings")
            .body_contains("\"model\":\"embed-model\"")
            .body_contains("\"prompt\":\"hello world\"");
        then.status(200)
            .json_body(json!({ "embedding": [1.0_f32, 0.0_f32] }));
    });

    let cfg = config_for_server(&server);
    let mut search = semantic_search_from_config(&cfg).expect("semantic search should build");
    search.index_project(&workspace());

    let results = search.search("hello world");
    assert!(!results.is_empty());
    assert_eq!(results[0].path, PathBuf::from("src/Hello.java"));

    // Ensure we only probe the batch endpoint once.
    missing_batch.assert_hits(1);
    hello_mock.assert_hits(2);
    other_mock.assert_hits(2);
    query_mock.assert_hits(1);
}
