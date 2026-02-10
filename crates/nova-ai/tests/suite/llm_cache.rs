use futures::{StreamExt, TryStreamExt};
use hyper::{
    service::{make_service_fn, service_fn},
    Body, Request, Response, Server, StatusCode,
};
use httpmock::prelude::*;
use nova_ai::{AiClient, ChatMessage, ChatRequest};
use nova_config::{AiConfig, AiProviderKind};
use serde_json::json;
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

fn http_config(url: Url, model: &str) -> AiConfig {
    let mut cfg = AiConfig::default();
    cfg.enabled = true;
    cfg.provider.kind = AiProviderKind::Http;
    cfg.provider.url = url;
    cfg.provider.model = model.to_string();
    cfg.provider.timeout_ms = 1_000;
    cfg.provider.concurrency = Some(1);
    cfg.provider.max_tokens = 64;
    cfg.privacy.local_only = false;
    cfg.privacy.anonymize_identifiers = Some(false);
    cfg.cache_enabled = true;
    cfg.cache_max_entries = 32;
    cfg.cache_ttl_secs = 60;
    cfg
}

fn openai_compatible_config(url: Url, model: &str) -> AiConfig {
    let mut cfg = AiConfig::default();
    cfg.enabled = true;
    cfg.provider.kind = AiProviderKind::OpenAiCompatible;
    cfg.provider.url = url;
    cfg.provider.model = model.to_string();
    cfg.provider.timeout_ms = 1_000;
    cfg.provider.concurrency = Some(1);
    cfg.provider.max_tokens = 64;
    cfg.privacy.local_only = false;
    cfg.privacy.anonymize_identifiers = Some(false);
    cfg.cache_enabled = true;
    cfg.cache_max_entries = 32;
    cfg.cache_ttl_secs = 60;
    cfg
}

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
async fn llm_chat_is_cached_for_identical_requests() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/complete");
        then.status(200).json_body(json!({ "completion": "Pong" }));
    });

    let cfg = http_config(
        Url::parse(&format!("{}/complete", server.base_url())).unwrap(),
        "default",
    );

    let client = AiClient::from_config(&cfg).unwrap();
    let request = ChatRequest {
        messages: vec![ChatMessage::user("Ping")],
        max_tokens: Some(5),
        temperature: Some(0.2),
    };

    let out1 = client
        .chat(request.clone(), CancellationToken::new())
        .await
        .unwrap();
    let out2 = client
        .chat(request, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(out1, "Pong");
    assert_eq!(out2, "Pong");
    mock.assert_hits(1);
}

#[tokio::test(flavor = "current_thread")]
async fn llm_chat_stream_is_cached_for_identical_requests() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/complete");
        then.status(200).json_body(json!({ "completion": "Pong" }));
    });

    let cfg = http_config(
        Url::parse(&format!("{}/complete", server.base_url())).unwrap(),
        "default",
    );

    let client = AiClient::from_config(&cfg).unwrap();
    let request = ChatRequest {
        messages: vec![ChatMessage::user("Ping")],
        max_tokens: Some(5),
        temperature: Some(0.2),
    };

    let out1: Vec<String> = client
        .chat_stream(request.clone(), CancellationToken::new())
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    let out2: Vec<String> = client
        .chat_stream(request, CancellationToken::new())
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();

    assert_eq!(out1.concat(), "Pong");
    assert_eq!(out2.concat(), "Pong");
    mock.assert_hits(1);
}

#[tokio::test(flavor = "current_thread")]
async fn llm_chat_stream_cancelled_midway_does_not_populate_cache() {
    let calls = Arc::new(AtomicUsize::new(0));
    let handler_calls = calls.clone();
    let handler = move |req: Request<Body>| {
        let handler_calls = handler_calls.clone();
        async move {
            if req.uri().path() != "/v1/chat/completions" {
                return Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .body(Body::empty())
                    .unwrap();
            }

            let _ = hyper::body::to_bytes(req.into_body())
                .await
                .expect("read request body");

            let call_index = handler_calls.fetch_add(1, Ordering::SeqCst);
            if call_index == 0 {
                let body_stream = async_stream::stream! {
                    yield Ok::<_, std::io::Error>(hyper::body::Bytes::from(
                        "data: {\"choices\":[{\"delta\":{\"content\":\"Po\"}}]}\n\n",
                    ));
                    futures::future::pending::<()>().await;
                };

                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "text/event-stream")
                    .body(Body::wrap_stream(body_stream))
                    .unwrap()
            } else {
                let chunks = vec![
                    Ok::<_, std::io::Error>(hyper::body::Bytes::from(
                        "data: {\"choices\":[{\"delta\":{\"content\":\"Pong\"}}]}\n\n",
                    )),
                    Ok::<_, std::io::Error>(hyper::body::Bytes::from("data: [DONE]\n\n")),
                ];
                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "text/event-stream")
                    .body(Body::wrap_stream(futures::stream::iter(chunks)))
                    .unwrap()
            }
        }
    };

    let (addr, handle) = spawn_server(handler);
    let cfg = openai_compatible_config(Url::parse(&format!("http://{addr}")).unwrap(), "default");
    let client = AiClient::from_config(&cfg).unwrap();

    let request = ChatRequest {
        messages: vec![ChatMessage::user("Ping")],
        max_tokens: Some(5),
        temperature: None,
    };

    let cancel = CancellationToken::new();
    let mut stream = client
        .chat_stream(request.clone(), cancel.clone())
        .await
        .unwrap();

    let first = stream
        .next()
        .await
        .expect("expected first chunk")
        .expect("expected first chunk ok");
    assert_eq!(first, "Po");

    cancel.cancel();
    let err = stream
        .next()
        .await
        .expect("expected cancellation error")
        .expect_err("expected cancellation error");
    assert!(matches!(err, nova_ai::AiError::Cancelled));
    drop(stream);

    let out: Vec<String> = client
        .chat_stream(request, CancellationToken::new())
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    assert_eq!(out.concat(), "Pong");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "expected second request to hit network (cancelled stream should not populate cache)"
    );

    handle.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn llm_cache_misses_when_model_changes() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/complete");
        then.status(200).json_body(json!({ "completion": "Pong" }));
    });

    let base_url = Url::parse(&format!("{}/complete", server.base_url())).unwrap();
    let request = ChatRequest {
        messages: vec![ChatMessage::user("Ping")],
        max_tokens: Some(5),
        temperature: Some(0.2),
    };

    let client_a = AiClient::from_config(&http_config(base_url.clone(), "model-a")).unwrap();
    assert_eq!(
        client_a
            .chat(request.clone(), CancellationToken::new())
            .await
            .unwrap(),
        "Pong"
    );

    let client_b = AiClient::from_config(&http_config(base_url, "model-b")).unwrap();
    assert_eq!(
        client_b
            .chat(request, CancellationToken::new())
            .await
            .unwrap(),
        "Pong"
    );

    // Both requests should hit the network because the model differs (keyed in the cache).
    mock.assert_hits(2);
}

#[tokio::test(flavor = "current_thread")]
async fn llm_cache_misses_when_temperature_changes() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/complete");
        then.status(200).json_body(json!({ "completion": "Pong" }));
    });

    let cfg = http_config(
        Url::parse(&format!("{}/complete", server.base_url())).unwrap(),
        "default",
    );
    let client = AiClient::from_config(&cfg).unwrap();

    let request_a = ChatRequest {
        messages: vec![ChatMessage::user("Ping")],
        max_tokens: Some(5),
        temperature: Some(0.2),
    };
    let request_b = ChatRequest {
        temperature: Some(0.3),
        ..request_a.clone()
    };

    assert_eq!(
        client
            .chat(request_a, CancellationToken::new())
            .await
            .unwrap(),
        "Pong"
    );
    assert_eq!(
        client
            .chat(request_b, CancellationToken::new())
            .await
            .unwrap(),
        "Pong"
    );

    mock.assert_hits(2);
}

#[tokio::test(flavor = "current_thread")]
async fn llm_cache_misses_when_temperature_is_none_vs_zero() {
    // Regression test: cache keys must distinguish "unset" temperature from an explicit `0.0`.
    // (Without encoding the option discriminant, `None` would collide with `Some(0.0)`.)
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/complete");
        then.status(200).json_body(json!({ "completion": "Pong" }));
    });

    let cfg = http_config(
        Url::parse(&format!("{}/complete", server.base_url())).unwrap(),
        "default",
    );
    let client = AiClient::from_config(&cfg).unwrap();

    let request_a = ChatRequest {
        messages: vec![ChatMessage::user("Ping")],
        max_tokens: Some(5),
        temperature: None,
    };
    let request_b = ChatRequest {
        temperature: Some(0.0),
        ..request_a.clone()
    };

    assert_eq!(
        client
            .chat(request_a, CancellationToken::new())
            .await
            .unwrap(),
        "Pong"
    );
    assert_eq!(
        client
            .chat(request_b, CancellationToken::new())
            .await
            .unwrap(),
        "Pong"
    );

    mock.assert_hits(2);
}
