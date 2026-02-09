#![cfg(all(feature = "embeddings", not(feature = "embeddings-local")))]

use nova_ai::embeddings::embeddings_client_from_config;
use nova_config::{AiConfig, AiEmbeddingsBackend};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn embeddings_client_from_config_local_backend_without_feature_falls_back_to_hash_embedder() {
    let mut config = AiConfig::default();
    config.enabled = true;
    config.embeddings.enabled = true;
    config.embeddings.backend = AiEmbeddingsBackend::Local;

    let client = embeddings_client_from_config(&config).expect("build embeddings client");
    let out = client
        .embed(&["hello world".to_string()], CancellationToken::new())
        .await
        .expect("embed");

    assert_eq!(out.len(), 1);
    assert_eq!(
        out[0].len(),
        256,
        "expected local backend to fall back to HashEmbedder when `embeddings-local` is disabled"
    );
}

