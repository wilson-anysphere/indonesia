use httpmock::prelude::*;
use nova_ai::embeddings::{embeddings_client_from_config, EmbeddingInputKind};
use nova_config::{AiConfig, AiEmbeddingsBackend, AiProviderKind};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use url::Url;

fn openai_compatible_config(server: &MockServer, model_dir: std::path::PathBuf) -> AiConfig {
    let mut cfg = AiConfig::default();
    cfg.enabled = true;
    cfg.embeddings.enabled = true;
    cfg.embeddings.backend = AiEmbeddingsBackend::Provider;
    cfg.embeddings.timeout_ms = Some(2_000);
    cfg.embeddings.model_dir = model_dir;
    cfg.provider.kind = AiProviderKind::OpenAiCompatible;
    cfg.provider.url = Url::parse(&server.base_url()).expect("valid url");
    cfg.provider.model = "test-embedding-model".to_string();
    cfg.provider.timeout_ms = 2_000;
    cfg
}

#[tokio::test]
async fn embeddings_retries_on_http_429() {
    let server = MockServer::start();
    let dir = tempfile::tempdir().expect("tempdir");
    let model_dir = dir.path().join("models").join("embeddings");

    let cfg = openai_compatible_config(&server, model_dir);
    let client = embeddings_client_from_config(&cfg).expect("client");

    let mut mock_429 = server.mock(|when, then| {
        when.method(POST).path("/v1/embeddings");
        then.status(429);
    });

    let task = {
        tokio::spawn(async move {
            client
                .embed(
                    &["hello".to_string()],
                    EmbeddingInputKind::Query,
                    CancellationToken::new(),
                )
                .await
        })
    };

    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while mock_429.hits() == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("first request observed");

    let hits_429 = mock_429.hits();
    assert_eq!(
        hits_429, 1,
        "expected the first embedding request to hit the 429 mock exactly once"
    );
    mock_429.delete();

    let mock_ok = server.mock(|when, then| {
        when.method(POST).path("/v1/embeddings");
        then.status(200).json_body(json!({
            "data": [
                { "object": "embedding", "index": 0, "embedding": [0.1, 0.2, 0.3] }
            ]
        }));
    });

    let embedding = task.await.expect("join task").expect("embed succeeds");

    assert_eq!(embedding, vec![vec![0.1, 0.2, 0.3]]);
    assert_eq!(hits_429, 1);
    mock_ok.assert_hits(1);
}
