use hyper::{
    service::{make_service_fn, service_fn},
    Body, Request, Response, Server, StatusCode,
};
use nova_ai::{AiClient, AiError, ChatMessage, ChatRequest};
use nova_config::{AiConfig, AiProviderKind};
use std::{
    convert::Infallible,
    net::SocketAddr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
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

fn http_config(url: Url) -> AiConfig {
    let mut cfg = AiConfig::default();
    cfg.enabled = true;
    cfg.provider.kind = AiProviderKind::Http;
    cfg.provider.url = url;
    cfg.provider.model = "test-model".to_string();
    cfg.provider.timeout_ms = 1_000;
    cfg.provider.concurrency = Some(1);
    cfg.provider.max_tokens = 32;
    cfg.cache_enabled = false;
    cfg
}

#[tokio::test(flavor = "current_thread")]
async fn max_retries_zero_disables_retries() {
    let request_count = Arc::new(AtomicUsize::new(0));
    let request_count_for_handler = request_count.clone();
    let handler = move |req: Request<Body>| {
        let request_count = request_count_for_handler.clone();
        async move {
            assert_eq!(req.method(), hyper::Method::POST);
            assert_eq!(req.uri().path(), "/complete");
            let _ = hyper::body::to_bytes(req.into_body()).await;

            let call = request_count.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Body::from("boom"))
                    .expect("response")
            } else {
                Response::new(Body::from(r#"{"completion":"Pong"}"#))
            }
        }
    };

    let (addr, handle) = spawn_server(handler);
    let url = Url::parse(&format!("http://{addr}/complete")).expect("url");

    let mut cfg = http_config(url);
    cfg.provider.retry_max_retries = 0;
    // Make any backoff config irrelevant and keep the test fast.
    cfg.provider.retry_initial_backoff_ms = 1;
    cfg.provider.retry_max_backoff_ms = 1;

    let client = AiClient::from_config(&cfg).expect("client");
    let err = client
        .chat(
            ChatRequest {
                messages: vec![ChatMessage::user("Ping")],
                max_tokens: Some(5),
                temperature: None,
            },
            CancellationToken::new(),
        )
        .await
        .expect_err("expected first request to fail without retries");
    assert!(matches!(err, AiError::Http(_)), "unexpected error: {err:?}");

    assert_eq!(
        request_count.load(Ordering::SeqCst),
        1,
        "expected a single request when retries are disabled"
    );

    handle.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn max_retries_allows_retry_on_500() {
    let request_count = Arc::new(AtomicUsize::new(0));
    let request_count_for_handler = request_count.clone();
    let handler = move |req: Request<Body>| {
        let request_count = request_count_for_handler.clone();
        async move {
            assert_eq!(req.method(), hyper::Method::POST);
            assert_eq!(req.uri().path(), "/complete");
            let _ = hyper::body::to_bytes(req.into_body()).await;

            let call = request_count.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Body::from("boom"))
                    .expect("response")
            } else {
                Response::new(Body::from(r#"{"completion":"Pong"}"#))
            }
        }
    };

    let (addr, handle) = spawn_server(handler);
    let url = Url::parse(&format!("http://{addr}/complete")).expect("url");

    let mut cfg = http_config(url);
    cfg.provider.retry_max_retries = 1;
    cfg.provider.retry_initial_backoff_ms = 1;
    cfg.provider.retry_max_backoff_ms = 1;

    let client = AiClient::from_config(&cfg).expect("client");
    let out = client
        .chat(
            ChatRequest {
                messages: vec![ChatMessage::user("Ping")],
                max_tokens: Some(5),
                temperature: None,
            },
            CancellationToken::new(),
        )
        .await
        .expect("expected retry to succeed");

    assert_eq!(out, "Pong");
    assert_eq!(
        request_count.load(Ordering::SeqCst),
        2,
        "expected one retry after the initial 500"
    );

    handle.abort();
}
