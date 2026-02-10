#![cfg(feature = "embeddings")]

use httpmock::prelude::*;
use nova_ai::semantic_search_from_config;
use nova_config::{AiConfig, AiEmbeddingsBackend, AiProviderKind};
use serde_json::json;
use std::path::PathBuf;
use url::Url;

#[test]
fn provider_semantic_search_embed_batch_uses_memory_and_disk_caches() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/v1/embeddings");
        then.status(200).json_body(json!({
            "data": [
                { "index": 0, "embedding": [1.0, 0.0, 0.0] },
                { "index": 1, "embedding": [1.0, 0.0, 0.0] },
                { "index": 2, "embedding": [1.0, 0.0, 0.0] },
            ],
        }));
    });

    let dir = tempfile::tempdir().expect("tempdir");
    let model_dir = dir.path().join("models").join("embeddings");

    let mut cfg = AiConfig::default();
    cfg.enabled = true;
    cfg.features.semantic_search = true;
    cfg.embeddings.enabled = true;
    cfg.embeddings.backend = AiEmbeddingsBackend::Provider;
    cfg.embeddings.model_dir = model_dir;
    cfg.provider.kind = AiProviderKind::OpenAiCompatible;
    cfg.provider.url = Url::parse(&format!("{}/v1", server.base_url())).expect("base url");
    cfg.provider.model = "text-embedding-3-small".to_string();
    cfg.provider.timeout_ms = 2_000;

    let path = PathBuf::from("src/Hello.java");
    let text = r#"
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
    .to_string();

    let mut search = semantic_search_from_config(&cfg).expect("semantic search should build");
    search.index_file(path.clone(), text.clone());

    mock.assert_hits(1);
    let cold_hits = mock.hits();

    // Re-indexing the exact same content should avoid additional HTTP calls by hitting the
    // in-memory embedding cache.
    search.index_file(path.clone(), text.clone());
    mock.assert_hits(cold_hits);

    drop(search);

    // Rebuilding the semantic search instance should retain embeddings on disk, avoiding network
    // calls on restart.
    let mut search = semantic_search_from_config(&cfg).expect("semantic search should build");
    let hits_before_restart = mock.hits();
    search.index_file(path, text);
    let hits_after_restart = mock.hits();

    let additional_hits = hits_after_restart.saturating_sub(hits_before_restart);
    assert!(
        additional_hits == 0 || additional_hits < cold_hits,
        "expected disk cache to eliminate (or significantly reduce) provider requests; \
         cold_hits={cold_hits}, additional_hits={additional_hits}"
    );
}
