use futures::StreamExt;
use hyper::{
    service::{make_service_fn, service_fn},
    Body, Request, Response, Server, StatusCode,
};
use nova_ai::{AiClient, AiError, ChatMessage, ChatRequest, CodeSnippet, NovaAi};
use nova_config::{AiConfig, AiPrivacyConfig, AiProviderConfig, AiProviderKind};
use serde_json::Value;
use std::{convert::Infallible, net::SocketAddr, time::Duration};
use tokio::{sync::mpsc, task::JoinHandle};
use futures::stream::iter;
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

fn openai_config(url: Url) -> AiConfig {
    AiConfig {
        provider: AiProviderConfig {
            kind: AiProviderKind::OpenAiCompatible,
            url,
            model: "test-model".to_string(),
            max_tokens: 128,
            timeout_ms: 500,
            concurrency: 1,
        },
        privacy: AiPrivacyConfig {
            local_only: true,
            anonymize: None,
            excluded_paths: vec![],
            redact_patterns: vec![],
        },
        enabled: true,
        ..AiConfig::default()
    }
}

fn ollama_config(url: Url) -> AiConfig {
    AiConfig {
        provider: AiProviderConfig {
            kind: AiProviderKind::Ollama,
            url,
            model: "llama3".to_string(),
            max_tokens: 128,
            timeout_ms: 500,
            concurrency: 1,
        },
        privacy: AiPrivacyConfig {
            local_only: true,
            anonymize: None,
            excluded_paths: vec![],
            redact_patterns: vec![],
        },
        enabled: true,
        ..AiConfig::default()
    }
}

#[test]
fn local_only_allows_loopback_urls() {
    let url = Url::parse("http://localhost:11434").expect("valid url");
    let config = openai_config(url);
    AiClient::from_config(&config).expect("localhost should be allowed in local-only mode");
}

#[test]
fn local_only_rejects_remote_urls() {
    let url = Url::parse("http://example.com").expect("valid url");
    let config = openai_config(url);
    let err = match AiClient::from_config(&config) {
        Ok(_) => panic!("remote urls must be rejected"),
        Err(err) => err,
    };
    match err {
        AiError::InvalidConfig(msg) => {
            assert!(msg.contains("local_only"), "error message should mention local_only");
            assert!(
                msg.contains("loopback") || msg.contains("localhost"),
                "error message should guide users toward loopback/localhost"
            );
        }
        other => panic!("expected InvalidConfig, got {other:?}"),
    }
}

#[test]
fn remote_urls_allowed_when_not_local_only() {
    let url = Url::parse("http://example.com").expect("valid url");
    let mut config = openai_config(url);
    config.privacy.local_only = false;
    AiClient::from_config(&config).expect("remote urls should be allowed when local_only=false");
}

#[tokio::test]
async fn openai_compatible_request_formatting() {
    let (body_tx, mut body_rx) = mpsc::channel::<Value>(1);

    let handler = move |req: Request<Body>| {
        let body_tx = body_tx.clone();
        async move {
            assert_eq!(req.method(), hyper::Method::POST);
            assert_eq!(req.uri().path(), "/v1/chat/completions");

            let bytes = hyper::body::to_bytes(req.into_body())
                .await
                .expect("read body");
            let json: Value = serde_json::from_slice(&bytes).expect("parse json");
            let _ = body_tx.send(json).await;

            Response::new(Body::from(
                r#"{"choices":[{"message":{"content":"hello"}}]}"#,
            ))
        }
    };

    let (addr, handle) = spawn_server(handler);
    let url = Url::parse(&format!("http://{addr}")).unwrap();
    let client = AiClient::from_config(&openai_config(url)).unwrap();

    let content = client
        .chat(
            ChatRequest {
                messages: vec![ChatMessage::user("hi")],
                max_tokens: Some(7),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(content, "hello");

    let body = body_rx.recv().await.expect("request body");
    assert_eq!(body["model"], "test-model");
    assert_eq!(body["max_tokens"], 7);
    assert_eq!(body["stream"], false);
    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(body["messages"][0]["content"], "hi");

    handle.abort();
}

#[tokio::test]
async fn ollama_request_formatting() {
    let (body_tx, mut body_rx) = mpsc::channel::<Value>(1);

    let handler = move |req: Request<Body>| {
        let body_tx = body_tx.clone();
        async move {
            assert_eq!(req.method(), hyper::Method::POST);
            assert_eq!(req.uri().path(), "/api/chat");

            let bytes = hyper::body::to_bytes(req.into_body())
                .await
                .expect("read body");
            let json: Value = serde_json::from_slice(&bytes).expect("parse json");
            let _ = body_tx.send(json).await;

            Response::new(Body::from(r#"{"message":{"content":"hello"},"done":true}"#))
        }
    };

    let (addr, handle) = spawn_server(handler);
    let url = Url::parse(&format!("http://{addr}")).unwrap();
    let client = AiClient::from_config(&ollama_config(url)).unwrap();

    let content = client
        .chat(
            ChatRequest {
                messages: vec![ChatMessage::user("hi")],
                max_tokens: Some(11),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(content, "hello");

    let body = body_rx.recv().await.expect("request body");
    assert_eq!(body["model"], "llama3");
    assert_eq!(body["stream"], false);
    assert_eq!(body["options"]["num_predict"], 11);
    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(body["messages"][0]["content"], "hi");

    handle.abort();
}

#[tokio::test]
async fn openai_compatible_streaming_parsing() {
    let (body_tx, mut body_rx) = mpsc::channel::<Value>(1);

    let handler = move |req: Request<Body>| {
        let body_tx = body_tx.clone();
        async move {
            if req.uri().path() != "/v1/chat/completions" {
                return Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .body(Body::empty())
                    .unwrap();
            }

            let bytes = hyper::body::to_bytes(req.into_body())
                .await
                .expect("read body");
            let json: Value = serde_json::from_slice(&bytes).expect("parse json");
            let _ = body_tx.send(json).await;

            let chunks = vec![
                Ok::<_, std::io::Error>(hyper::body::Bytes::from(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n",
                )),
                Ok::<_, std::io::Error>(hyper::body::Bytes::from(
                    "data: {\"choices\":[{\"delta\":{\"content\":\" world\"}}]}\n\n",
                )),
                Ok::<_, std::io::Error>(hyper::body::Bytes::from("data: [DONE]\n\n")),
            ];

            Response::builder()
                .header("content-type", "text/event-stream")
                .body(Body::wrap_stream(iter(chunks)))
                .unwrap()
        }
    };

    let (addr, handle) = spawn_server(handler);
    let url = Url::parse(&format!("http://{addr}")).unwrap();
    let client = AiClient::from_config(&openai_config(url)).unwrap();

    let mut stream = client
        .chat_stream(
            ChatRequest {
                messages: vec![ChatMessage::user("hi")],
                max_tokens: Some(5),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let mut output = String::new();
    while let Some(item) = stream.next().await {
        output.push_str(&item.unwrap());
    }
    assert_eq!(output, "Hello world");

    let body = body_rx.recv().await.expect("request body");
    assert_eq!(body["stream"], true);

    handle.abort();
}

#[tokio::test]
async fn cancellation_stops_in_flight_request() {
    let (started_tx, mut started_rx) = mpsc::channel::<()>(1);

    let handler = move |req: Request<Body>| {
        let started_tx = started_tx.clone();
        async move {
            assert_eq!(req.uri().path(), "/v1/chat/completions");
            let _ = started_tx.send(()).await;
            tokio::time::sleep(Duration::from_secs(5)).await;
            Response::new(Body::from(
                r#"{"choices":[{"message":{"content":"never"}}]}"#,
            ))
        }
    };

    let (addr, handle) = spawn_server(handler);
    let url = Url::parse(&format!("http://{addr}")).unwrap();

    let mut config = openai_config(url);
    config.provider.timeout_ms = 10_000;
    let client = AiClient::from_config(&config).unwrap();

    let cancel = CancellationToken::new();
    let cancel_task = {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let _ = started_rx.recv().await;
            cancel.cancel();
        })
    };

    let result = tokio::time::timeout(
        Duration::from_secs(1),
        client.chat(
            ChatRequest {
                messages: vec![ChatMessage::user("hi")],
                max_tokens: Some(5),
            },
            cancel,
        ),
    )
    .await
    .expect("client should return quickly");

    assert!(matches!(result, Err(AiError::Cancelled)));

    cancel_task.abort();
    handle.abort();
}

#[tokio::test]
async fn ai_actions_work_end_to_end_with_local_backend() {
    let (body_tx, mut body_rx) = mpsc::channel::<Value>(1);

    let handler = move |req: Request<Body>| {
        let body_tx = body_tx.clone();
        async move {
            if req.uri().path() != "/v1/chat/completions" {
                return Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .body(Body::empty())
                    .unwrap();
            }

            let bytes = hyper::body::to_bytes(req.into_body())
                .await
                .expect("read body");
            let json: Value = serde_json::from_slice(&bytes).expect("parse json");
            let _ = body_tx.send(json).await;

            Response::new(Body::from(
                r#"{"choices":[{"message":{"content":"explanation"}}]}"#,
            ))
        }
    };

    let (addr, handle) = spawn_server(handler);
    let url = Url::parse(&format!("http://{addr}")).unwrap();

    let ai = NovaAi::new(&openai_config(url)).unwrap();
    let response = ai
        .explain_error(
            "cannot find symbol",
            &CodeSnippet::ad_hoc("class A { void m(){ x(); } }"),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(response, "explanation");

    let body = body_rx.recv().await.expect("request body");
    assert_eq!(body["model"], "test-model");
    assert!(body["messages"][1]["content"]
        .as_str()
        .unwrap()
        .contains("cannot find symbol"));

    handle.abort();
}
