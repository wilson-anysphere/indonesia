#![cfg(feature = "embeddings")]

use hyper::{
    service::{make_service_fn, service_fn},
    Body, Request, Response, Server,
};
use nova_ai::embeddings::{embeddings_client_from_config, EmbeddingInputKind};
use nova_config::{AiConfig, AiEmbeddingsBackend, AiProviderKind};
use serde_json::{json, Value};
use std::{convert::Infallible, net::SocketAddr};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use url::Url;

fn spawn_server<F, Fut>(handler: F) -> (SocketAddr, JoinHandle<()>)
where
    F: Fn(Request<Body>) -> Fut + Clone + Send + 'static,
    Fut: std::future::Future<Output = Response<Body>> + Send + 'static,
{
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test server");
    let addr = listener.local_addr().expect("server addr");
    listener
        .set_nonblocking(true)
        .expect("set nonblocking listener");

    let make_svc = make_service_fn(move |_conn| {
        let handler = handler.clone();
        async move {
            Ok::<_, Infallible>(service_fn(move |req| {
                let handler = handler.clone();
                async move { Ok::<_, Infallible>(handler(req).await) }
            }))
        }
    });

    let server = Server::from_tcp(listener)
        .expect("create server")
        .serve(make_svc);

    let handle = tokio::spawn(async move {
        let _ = server.await;
    });

    (addr, handle)
}

#[tokio::test(flavor = "current_thread")]
async fn provider_embeddings_redacts_privacy_patterns_before_sending_to_http_backend() {
    let handler = move |req: Request<Body>| async move {
        assert_eq!(req.uri().path(), "/v1/embeddings");
        let bytes = hyper::body::to_bytes(req.into_body()).await.unwrap();
        let request_json: Value = serde_json::from_slice(&bytes).unwrap();

        let inputs = request_json["input"]
            .as_array()
            .expect("expected input to be an array of strings");

        // Ensure we never send raw secrets over provider-backed embeddings.
        for item in inputs {
            let text = item.as_str().expect("input items should be strings");
            assert!(
                !text.contains("supersecret"),
                "unsanitized embedding text leaked"
            );
            assert!(
                text.contains("[REDACTED]"),
                "expected privacy redaction marker in embedding text"
            );
        }

        let data = inputs
            .iter()
            .enumerate()
            .map(|(idx, _)| json!({ "index": idx, "embedding": [0.0, 0.1] }))
            .collect::<Vec<_>>();

        Response::new(Body::from(json!({ "data": data }).to_string()))
    };

    let (addr, handle) = spawn_server(handler);

    let mut cfg = AiConfig::default();
    cfg.enabled = true;
    cfg.embeddings.enabled = true;
    cfg.embeddings.backend = AiEmbeddingsBackend::Provider;
    cfg.provider.kind = AiProviderKind::OpenAiCompatible;
    cfg.provider.url = Url::parse(&format!("http://{addr}")).unwrap();
    cfg.provider.model = "test-embedding-model".to_string();
    cfg.provider.timeout_ms = 1_000;
    cfg.provider.concurrency = Some(1);

    // Force cloud mode (so provider-backed embeddings are allowed) but disable the default
    // anonymizer so this test can assert on regex redaction output deterministically.
    cfg.privacy.local_only = false;
    cfg.privacy.anonymize_identifiers = Some(false);
    cfg.privacy.redact_sensitive_strings = Some(false);
    cfg.privacy.redact_numeric_literals = Some(false);
    cfg.privacy.strip_or_redact_comments = Some(false);
    cfg.privacy.redact_patterns = vec!["supersecret".to_string()];

    let client = embeddings_client_from_config(&cfg).unwrap();

    // Document/code-like input.
    let docs = vec!["class Demo { int supersecret = 1; }".to_string()];
    let out = client
        .embed(&docs, EmbeddingInputKind::Document, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(out.len(), docs.len());

    // Natural language query input.
    let queries = vec!["where is supersecret declared?".to_string()];
    let out = client
        .embed(&queries, EmbeddingInputKind::Query, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(out.len(), queries.len());

    handle.abort();
}
