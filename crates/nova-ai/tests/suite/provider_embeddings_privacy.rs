#![cfg(feature = "embeddings")]

use httpmock::prelude::*;
use nova_ai::embeddings::{embeddings_client_from_config, EmbeddingInputKind};
use nova_ai::{semantic_search_from_config, VirtualWorkspace};
use nova_config::{AiConfig, AiEmbeddingsBackend, AiProviderKind};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use url::Url;

#[test]
fn provider_embeddings_remote_url_falls_back_to_hash_in_local_only_mode() {
    let db = VirtualWorkspace::new([(
        "src/Hello.java".to_string(),
        r#"
            public class Hello {
                public String helloWorld() {
                    return "hello world";
                }
            }
        "#
        .to_string(),
    )]);

    let mut cfg = AiConfig::default();
    cfg.enabled = true;
    cfg.features.semantic_search = true;
    cfg.embeddings.enabled = true;
    cfg.embeddings.backend = AiEmbeddingsBackend::Provider;
    cfg.provider.kind = AiProviderKind::OpenAiCompatible;
    cfg.provider.url = Url::parse("https://api.openai.com/v1").unwrap();
    cfg.provider.model = "text-embedding-3-small".to_string();

    // Must not panic or attempt a remote request in local-only mode. The embedder should fall back
    // to the local hash embedder and still return embedding-backed results.
    let mut search = semantic_search_from_config(&cfg).expect("semantic search should build");
    search.index_project(&db);
    let results = search.search("hello world");
    assert!(!results.is_empty());
    assert_eq!(results[0].kind, "method");
}

#[test]
fn provider_embeddings_loopback_url_is_allowed_in_local_only_mode() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/embeddings")
            .body_contains("\"input\"")
            .body_contains("\"model\"");
        then.status(200).json_body(json!({
            "data": [{
                "embedding": [1.0, 0.0, 0.0],
                "index": 0,
                "object": "embedding"
            }]
        }));
    });

    let db = VirtualWorkspace::new([(
        "src/Hello.java".to_string(),
        r#"
            public class Hello {
                public String helloWorld() {
                    return "hello world";
                }
            }
        "#
        .to_string(),
    )]);

    let mut cfg = AiConfig::default();
    cfg.enabled = true;
    cfg.features.semantic_search = true;
    cfg.embeddings.enabled = true;
    cfg.embeddings.backend = AiEmbeddingsBackend::Provider;
    cfg.provider.kind = AiProviderKind::OpenAiCompatible;
    cfg.provider.url =
        Url::parse(&format!("http://localhost:{}/v1", server.port())).unwrap();
    cfg.provider.model = "test-embed-model".to_string();

    let mut search = semantic_search_from_config(&cfg).expect("semantic search should build");
    search.index_project(&db);
    let results = search.search("hello world");
    assert!(!results.is_empty());
    assert_eq!(results[0].kind, "method");

    let hits = mock.hits();
    assert!(
        hits >= 2,
        "expected at least 2 embedding requests (index + query), got {hits}"
    );
}

#[tokio::test]
async fn provider_embeddings_client_remote_url_falls_back_to_hash_in_local_only_mode() {
    let mut cfg = AiConfig::default();
    cfg.enabled = true;
    cfg.embeddings.enabled = true;
    cfg.embeddings.backend = AiEmbeddingsBackend::Provider;
    cfg.provider.kind = AiProviderKind::OpenAiCompatible;
    cfg.provider.url = Url::parse("https://api.openai.com/v1").unwrap();
    cfg.provider.model = "text-embedding-3-small".to_string();
    cfg.provider.timeout_ms = 10;

    let client = embeddings_client_from_config(&cfg).expect("embeddings client");
    let out = client
        .embed(
            &["hello".to_string()],
            EmbeddingInputKind::Query,
            CancellationToken::new(),
        )
        .await
        .expect("embed");

    assert_eq!(out.len(), 1);
    assert_eq!(out[0].len(), 256, "expected hash embedder fallback");
}

#[tokio::test]
async fn provider_embeddings_client_loopback_url_is_allowed_in_local_only_mode() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/v1/embeddings");
        then.status(200).json_body(json!({
            "data": [{
                "embedding": [1.0, 2.0, 3.0],
                "index": 0
            }]
        }));
    });

    let mut cfg = AiConfig::default();
    cfg.enabled = true;
    cfg.embeddings.enabled = true;
    cfg.embeddings.backend = AiEmbeddingsBackend::Provider;
    cfg.provider.kind = AiProviderKind::OpenAiCompatible;
    cfg.provider.url = Url::parse(&format!("http://localhost:{}/v1", server.port())).unwrap();
    cfg.provider.model = "test-embed-model".to_string();
    cfg.provider.timeout_ms = 1_000;

    let client = embeddings_client_from_config(&cfg).expect("embeddings client");
    let out = client
        .embed(
            &["hello".to_string()],
            EmbeddingInputKind::Query,
            CancellationToken::new(),
        )
        .await
        .expect("embed");

    mock.assert_hits(1);
    assert_eq!(out, vec![vec![1.0, 2.0, 3.0]]);
}
