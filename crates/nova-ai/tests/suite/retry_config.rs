use futures::TryStreamExt;
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
use std::io;
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

fn openai_compatible_config(url: Url) -> AiConfig {
    let mut cfg = AiConfig::default();
    cfg.enabled = true;
    cfg.provider.kind = AiProviderKind::OpenAiCompatible;
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

#[tokio::test(flavor = "current_thread")]
async fn chat_stream_retries_before_first_chunk_on_500() {
    let request_count = Arc::new(AtomicUsize::new(0));
    let request_count_for_handler = request_count.clone();
    let handler = move |req: Request<Body>| {
        let request_count = request_count_for_handler.clone();
        async move {
            assert_eq!(req.method(), hyper::Method::POST);
            assert_eq!(req.uri().path(), "/v1/chat/completions");
            let _ = hyper::body::to_bytes(req.into_body()).await;

            let call = request_count.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Body::from("boom"))
                    .expect("response")
            } else {
                let sse = concat!(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"P\"}}]}\n\n",
                    "data: {\"choices\":[{\"delta\":{\"content\":\"ong\"}}]}\n\n",
                    "data: [DONE]\n\n",
                );
                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "text/event-stream")
                    .body(Body::from(sse))
                    .expect("response")
            }
        }
    };

    let (addr, handle) = spawn_server(handler);
    let url = Url::parse(&format!("http://{addr}")).expect("url");

    let mut cfg = openai_compatible_config(url);
    cfg.provider.retry_max_retries = 1;
    cfg.provider.retry_initial_backoff_ms = 1;
    cfg.provider.retry_max_backoff_ms = 1;

    let client = AiClient::from_config(&cfg).expect("client");
    let stream = client
        .chat_stream(
            ChatRequest {
                messages: vec![ChatMessage::user("Ping")],
                max_tokens: Some(5),
                temperature: None,
            },
            CancellationToken::new(),
        )
        .await
        .expect("expected retry to establish stream");

    let chunks: Vec<String> = stream.try_collect().await.expect("stream ok");
    assert_eq!(chunks.concat(), "Pong");
    assert_eq!(
        request_count.load(Ordering::SeqCst),
        2,
        "expected one retry after the initial 500"
    );

    handle.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn chat_stream_retries_before_first_chunk_on_429() {
    let request_count = Arc::new(AtomicUsize::new(0));
    let request_count_for_handler = request_count.clone();
    let handler = move |req: Request<Body>| {
        let request_count = request_count_for_handler.clone();
        async move {
            assert_eq!(req.method(), hyper::Method::POST);
            assert_eq!(req.uri().path(), "/v1/chat/completions");
            let _ = hyper::body::to_bytes(req.into_body()).await;

            let call = request_count.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                Response::builder()
                    .status(StatusCode::TOO_MANY_REQUESTS)
                    .body(Body::from("slow down"))
                    .expect("response")
            } else {
                let sse = concat!(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"Pong\"}}]}\n\n",
                    "data: [DONE]\n\n",
                );
                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "text/event-stream")
                    .body(Body::from(sse))
                    .expect("response")
            }
        }
    };

    let (addr, handle) = spawn_server(handler);
    let url = Url::parse(&format!("http://{addr}")).expect("url");

    let mut cfg = openai_compatible_config(url);
    cfg.provider.retry_max_retries = 1;
    cfg.provider.retry_initial_backoff_ms = 1;
    cfg.provider.retry_max_backoff_ms = 1;

    let client = AiClient::from_config(&cfg).expect("client");
    let stream = client
        .chat_stream(
            ChatRequest {
                messages: vec![ChatMessage::user("Ping")],
                max_tokens: Some(5),
                temperature: None,
            },
            CancellationToken::new(),
        )
        .await
        .expect("expected retry to establish stream");

    let chunks: Vec<String> = stream.try_collect().await.expect("stream ok");
    assert_eq!(chunks.concat(), "Pong");
    assert_eq!(
        request_count.load(Ordering::SeqCst),
        2,
        "expected one retry after the initial 429"
    );

    handle.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn chat_stream_does_not_retry_after_first_chunk() {
    let request_count = Arc::new(AtomicUsize::new(0));
    let request_count_for_handler = request_count.clone();
    let handler = move |req: Request<Body>| {
        let request_count = request_count_for_handler.clone();
        async move {
            assert_eq!(req.method(), hyper::Method::POST);
            assert_eq!(req.uri().path(), "/v1/chat/completions");
            let _ = hyper::body::to_bytes(req.into_body()).await;

            request_count.fetch_add(1, Ordering::SeqCst);

            // Emit a single valid chunk, then abort the body stream with an I/O error. This
            // produces a retriable transport error *after* we yielded partial output; the client
            // must not retry in that case.
            let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"P\"}}]}\n\n";
            let body = Body::wrap_stream(async_stream::stream! {
                yield Ok::<_, io::Error>(sse.to_string());
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                yield Err(io::Error::new(io::ErrorKind::ConnectionReset, "boom"));
            });
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .body(body)
                .expect("response")
        }
    };

    let (addr, handle) = spawn_server(handler);
    let url = Url::parse(&format!("http://{addr}")).expect("url");

    let mut cfg = openai_compatible_config(url);
    cfg.provider.retry_max_retries = 3;
    cfg.provider.retry_initial_backoff_ms = 1;
    cfg.provider.retry_max_backoff_ms = 1;
    // Leave some slack for the server to deliver the first chunk before it aborts the body.
    cfg.provider.timeout_ms = 1_000;

    let client = AiClient::from_config(&cfg).expect("client");
    let mut stream = client
        .chat_stream(
            ChatRequest {
                messages: vec![ChatMessage::user("Ping")],
                max_tokens: Some(5),
                temperature: None,
            },
            CancellationToken::new(),
        )
        .await
        .expect("stream starts");

    let first = stream
        .try_next()
        .await
        .expect("stream yields first item")
        .expect("expected first chunk");
    assert_eq!(first, "P");

    let err = stream.try_next().await.expect_err("expected stream to error");
    assert!(matches!(err, AiError::Http(_)), "expected transport error; got {err:?}");

    assert_eq!(
        request_count.load(Ordering::SeqCst),
        1,
        "expected no retries after streaming has begun"
    );

    handle.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn chat_stream_max_retries_zero_disables_retries() {
    let request_count = Arc::new(AtomicUsize::new(0));
    let request_count_for_handler = request_count.clone();
    let handler = move |req: Request<Body>| {
        let request_count = request_count_for_handler.clone();
        async move {
            assert_eq!(req.method(), hyper::Method::POST);
            assert_eq!(req.uri().path(), "/v1/chat/completions");
            let _ = hyper::body::to_bytes(req.into_body()).await;

            let call = request_count.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Body::from("boom"))
                    .expect("response")
            } else {
                let sse = concat!(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"Pong\"}}]}\n\n",
                    "data: [DONE]\n\n",
                );
                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "text/event-stream")
                    .body(Body::from(sse))
                    .expect("response")
            }
        }
    };

    let (addr, handle) = spawn_server(handler);
    let url = Url::parse(&format!("http://{addr}")).expect("url");

    let mut cfg = openai_compatible_config(url);
    cfg.provider.retry_max_retries = 0;
    cfg.provider.retry_initial_backoff_ms = 1;
    cfg.provider.retry_max_backoff_ms = 1;

    let client = AiClient::from_config(&cfg).expect("client");
    let err = match client
        .chat_stream(
            ChatRequest {
                messages: vec![ChatMessage::user("Ping")],
                max_tokens: Some(5),
                temperature: None,
            },
            CancellationToken::new(),
        )
        .await
    {
        Ok(_) => panic!("expected stream to fail without retries"),
        Err(err) => err,
    };

    assert!(matches!(err, AiError::Http(_)), "unexpected error: {err:?}");
    assert_eq!(
        request_count.load(Ordering::SeqCst),
        1,
        "expected a single request when retries are disabled"
    );

    handle.abort();
}
