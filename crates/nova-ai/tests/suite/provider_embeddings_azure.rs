#![cfg(feature = "embeddings")]

use httpmock::prelude::*;
use nova_ai::embeddings::{embeddings_client_from_config, EmbeddingInputKind};
use nova_config::{AiConfig, AiEmbeddingsBackend, AiProviderKind};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use url::Url;

#[tokio::test]
async fn azure_openai_provider_embeddings_hits_deployment_embeddings_endpoint() {
    let server = MockServer::start();

    let mock = server.mock(|when, then| {
        when.method(POST)
            .path("/openai/deployments/my-deployment/embeddings")
            .query_param("api-version", "2024-02-01")
            .header("api-key", "test-key");
        then.status(200).json_body(json!({
            "object": "list",
            "data": [
                {
                    "object": "embedding",
                    "embedding": [1.0, 2.0, 3.0],
                    "index": 0
                }
            ],
            "model": "ignored",
            "usage": { "prompt_tokens": 1, "total_tokens": 1 }
        }));
    });

    let mut config = AiConfig::default();
    config.enabled = true;
    config.embeddings.enabled = true;
    config.embeddings.backend = AiEmbeddingsBackend::Provider;
    config.provider.kind = AiProviderKind::AzureOpenAi;
    config.provider.url = Url::parse(&format!("{}/", server.base_url())).unwrap();
    config.provider.azure_deployment = Some("my-deployment".to_string());
    config.api_key = Some("test-key".to_string());
    config.privacy.local_only = false;

    let client = embeddings_client_from_config(&config).expect("build embeddings client");
    let out = client
        .embed(
            &["hello".to_string()],
            EmbeddingInputKind::Query,
            CancellationToken::new(),
        )
        .await
        .expect("embed");

    mock.assert();
    assert_eq!(out, vec![vec![1.0, 2.0, 3.0]]);
}
