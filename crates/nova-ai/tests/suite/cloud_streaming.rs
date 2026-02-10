use futures::StreamExt;
use hyper::{
    service::{make_service_fn, service_fn},
    Body, Request, Response, Server, StatusCode,
};
use nova_ai::{AiClient, AiError, ChatMessage, ChatRequest};
use nova_config::{AiConfig, AiProviderKind};
use serde_json::Value;
use std::{convert::Infallible, net::SocketAddr, time::Duration};
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

fn base_config(kind: AiProviderKind, url: Url, model: &str) -> AiConfig {
    let mut cfg = AiConfig::default();
    cfg.enabled = true;
    cfg.provider.kind = kind;
    cfg.provider.url = url;
    cfg.provider.model = model.to_string();
    cfg.provider.max_tokens = 128;
    cfg.provider.timeout_ms = 1_000;
    cfg.provider.concurrency = Some(1);
    cfg.privacy.local_only = false;
    cfg.privacy.anonymize_identifiers = Some(false);
    cfg.cache_enabled = false;
    cfg
}

#[tokio::test(flavor = "current_thread")]
async fn anthropic_chat_stream_yields_text_deltas() {
    let handler = move |req: Request<Body>| async move {
        assert_eq!(req.method(), hyper::Method::POST);
        assert_eq!(req.uri().path(), "/v1/messages");

        assert_eq!(
            req.headers()
                .get("x-api-key")
                .and_then(|v| v.to_str().ok())
                .unwrap(),
            "test-key"
        );
        assert_eq!(
            req.headers()
                .get("anthropic-version")
                .and_then(|v| v.to_str().ok())
                .unwrap(),
            "2023-06-01"
        );

        let bytes = hyper::body::to_bytes(req.into_body())
            .await
            .expect("read request body");
        let json: Value = serde_json::from_slice(&bytes).expect("parse json");
        assert_eq!(json["stream"], true);

        let chunks = vec![
            Ok::<_, std::io::Error>(hyper::body::Bytes::from(
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"Hello\"}}\n\n",
            )),
            Ok::<_, std::io::Error>(hyper::body::Bytes::from(
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\" world\"}}\n\n",
            )),
            Ok::<_, std::io::Error>(hyper::body::Bytes::from(
                "event: message_stop\ndata: {}\n\n",
            )),
        ];

        Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::wrap_stream(futures::stream::iter(chunks)))
            .unwrap()
    };

    let (addr, handle) = spawn_server(handler);
    let mut cfg = base_config(
        AiProviderKind::Anthropic,
        Url::parse(&format!("http://{addr}")).unwrap(),
        "claude-3-5-sonnet-latest",
    );
    cfg.api_key = Some("test-key".to_string());

    let client = AiClient::from_config(&cfg).unwrap();
    let mut stream = client
        .chat_stream(
            ChatRequest {
                messages: vec![ChatMessage::user("hi")],
                max_tokens: Some(5),
                temperature: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let mut parts = Vec::new();
    while let Some(item) = stream.next().await {
        parts.push(item.unwrap());
    }
    assert_eq!(parts, vec!["Hello".to_string(), " world".to_string()]);
    assert_eq!(parts.concat(), "Hello world");

    handle.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn anthropic_chat_stream_timeout_is_idle_based_not_total_duration() {
    let handler = move |req: Request<Body>| async move {
        if req.uri().path() != "/v1/messages" {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())
                .unwrap();
        }

        let bytes = hyper::body::to_bytes(req.into_body())
            .await
            .expect("read request body");
        let json: Value = serde_json::from_slice(&bytes).expect("parse json");
        assert_eq!(json["stream"], true);

        // Send chunks frequently enough to stay under the provider idle timeout,
        // but allow the total stream duration to exceed it.
        let body_stream = async_stream::stream! {
            let parts = [
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"A\"}}\n\n",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"B\"}}\n\n",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"C\"}}\n\n",
                "event: message_stop\ndata: {}\n\n",
            ];

            for (idx, part) in parts.into_iter().enumerate() {
                if idx != 0 {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                yield Ok::<_, std::io::Error>(hyper::body::Bytes::from(part));
            }
        };

        Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::wrap_stream(body_stream))
            .unwrap()
    };

    let (addr, handle) = spawn_server(handler);
    let mut cfg = base_config(
        AiProviderKind::Anthropic,
        Url::parse(&format!("http://{addr}")).unwrap(),
        "claude-3-5-sonnet-latest",
    );
    cfg.api_key = Some("test-key".to_string());
    cfg.provider.timeout_ms = 100;

    let client = AiClient::from_config(&cfg).unwrap();
    let mut stream = client
        .chat_stream(
            ChatRequest {
                messages: vec![ChatMessage::user("hi")],
                max_tokens: Some(5),
                temperature: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let mut output = String::new();
    while let Some(item) = stream.next().await {
        output.push_str(&item.unwrap());
    }

    assert_eq!(output, "ABC");

    handle.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn gemini_chat_stream_yields_text_deltas() {
    let handler = move |req: Request<Body>| async move {
        assert_eq!(req.method(), hyper::Method::POST);
        assert_eq!(
            req.uri().path(),
            "/v1beta/models/gemini-1.5-flash:streamGenerateContent"
        );

        let query = req.uri().query().unwrap_or_default();
        assert!(query.contains("key=test-key"));
        if query.contains("alt=") {
            assert!(query.contains("alt=sse"));
        }

        let chunks = vec![
            Ok::<_, std::io::Error>(hyper::body::Bytes::from(
                "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hello\"}]}}]}\n\n",
            )),
            Ok::<_, std::io::Error>(hyper::body::Bytes::from(
                "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\" world\"}]}}]}\n\n",
            )),
        ];

        Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::wrap_stream(futures::stream::iter(chunks)))
            .unwrap()
    };

    let (addr, handle) = spawn_server(handler);
    let mut cfg = base_config(
        AiProviderKind::Gemini,
        Url::parse(&format!("http://{addr}")).unwrap(),
        "gemini-1.5-flash",
    );
    cfg.api_key = Some("test-key".to_string());

    let client = AiClient::from_config(&cfg).unwrap();
    let mut stream = client
        .chat_stream(
            ChatRequest {
                messages: vec![ChatMessage::user("hi")],
                max_tokens: Some(5),
                temperature: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let mut parts = Vec::new();
    while let Some(item) = stream.next().await {
        parts.push(item.unwrap());
    }
    assert_eq!(parts, vec!["Hello".to_string(), " world".to_string()]);
    assert_eq!(parts.concat(), "Hello world");

    handle.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn gemini_chat_stream_timeout_is_idle_based_not_total_duration() {
    let handler = move |req: Request<Body>| async move {
        if req.uri().path() != "/v1beta/models/gemini-1.5-flash:streamGenerateContent" {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())
                .unwrap();
        }

        let query = req.uri().query().unwrap_or_default();
        assert!(query.contains("key=test-key"));
        if query.contains("alt=") {
            assert!(query.contains("alt=sse"));
        }

        let _ = hyper::body::to_bytes(req.into_body())
            .await
            .expect("read request body");

        let body_stream = async_stream::stream! {
            let parts = [
                "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"A\"}]}}]}\n\n",
                "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"B\"}]}}]}\n\n",
                "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"C\"}]}}]}\n\n",
                "data: [DONE]\n\n",
            ];

            for (idx, part) in parts.into_iter().enumerate() {
                if idx != 0 {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                yield Ok::<_, std::io::Error>(hyper::body::Bytes::from(part));
            }
        };

        Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::wrap_stream(body_stream))
            .unwrap()
    };

    let (addr, handle) = spawn_server(handler);
    let mut cfg = base_config(
        AiProviderKind::Gemini,
        Url::parse(&format!("http://{addr}")).unwrap(),
        "gemini-1.5-flash",
    );
    cfg.api_key = Some("test-key".to_string());
    cfg.provider.timeout_ms = 100;

    let client = AiClient::from_config(&cfg).unwrap();
    let mut stream = client
        .chat_stream(
            ChatRequest {
                messages: vec![ChatMessage::user("hi")],
                max_tokens: Some(5),
                temperature: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let mut output = String::new();
    while let Some(item) = stream.next().await {
        output.push_str(&item.unwrap());
    }

    assert_eq!(output, "ABC");

    handle.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn azure_openai_chat_stream_yields_text_deltas() {
    let handler = move |req: Request<Body>| async move {
        assert_eq!(req.method(), hyper::Method::POST);
        assert_eq!(
            req.uri().path(),
            "/openai/deployments/my-deployment/chat/completions"
        );
        assert!(req
            .uri()
            .query()
            .unwrap_or_default()
            .contains("api-version=2024-02-01"));

        assert_eq!(
            req.headers()
                .get("api-key")
                .and_then(|v| v.to_str().ok())
                .unwrap(),
            "test-key"
        );

        let bytes = hyper::body::to_bytes(req.into_body())
            .await
            .expect("read request body");
        let json: Value = serde_json::from_slice(&bytes).expect("parse json");
        assert_eq!(json["stream"], true);

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
            .body(Body::wrap_stream(futures::stream::iter(chunks)))
            .unwrap()
    };

    let (addr, handle) = spawn_server(handler);
    let mut cfg = base_config(
        AiProviderKind::AzureOpenAi,
        Url::parse(&format!("http://{addr}")).unwrap(),
        "unused",
    );
    cfg.api_key = Some("test-key".to_string());
    cfg.provider.azure_deployment = Some("my-deployment".to_string());
    cfg.provider.azure_api_version = Some("2024-02-01".to_string());

    let client = AiClient::from_config(&cfg).unwrap();
    let mut stream = client
        .chat_stream(
            ChatRequest {
                messages: vec![ChatMessage::user("hi")],
                max_tokens: Some(5),
                temperature: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let mut parts = Vec::new();
    while let Some(item) = stream.next().await {
        parts.push(item.unwrap());
    }
    assert_eq!(parts, vec!["Hello".to_string(), " world".to_string()]);
    assert_eq!(parts.concat(), "Hello world");

    handle.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn azure_openai_chat_stream_timeout_is_idle_based_not_total_duration() {
    let handler = move |req: Request<Body>| async move {
        if req.uri().path() != "/openai/deployments/my-deployment/chat/completions" {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())
                .unwrap();
        }

        let bytes = hyper::body::to_bytes(req.into_body())
            .await
            .expect("read request body");
        let json: Value = serde_json::from_slice(&bytes).expect("parse json");
        assert_eq!(json["stream"], true);

        // Send chunks frequently enough to stay under the provider idle timeout,
        // but allow the total stream duration to exceed it.
        let body_stream = async_stream::stream! {
            let parts = [
                "data: {\"choices\":[{\"delta\":{\"content\":\"A\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"B\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"C\"}}]}\n\n",
                "data: [DONE]\n\n",
            ];

            for (idx, part) in parts.into_iter().enumerate() {
                if idx != 0 {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                yield Ok::<_, std::io::Error>(hyper::body::Bytes::from(part));
            }
        };

        Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::wrap_stream(body_stream))
            .unwrap()
    };

    let (addr, handle) = spawn_server(handler);
    let mut cfg = base_config(
        AiProviderKind::AzureOpenAi,
        Url::parse(&format!("http://{addr}")).unwrap(),
        "unused",
    );
    cfg.api_key = Some("test-key".to_string());
    cfg.provider.azure_deployment = Some("my-deployment".to_string());
    cfg.provider.azure_api_version = Some("2024-02-01".to_string());
    cfg.provider.timeout_ms = 100;

    let client = AiClient::from_config(&cfg).unwrap();
    let mut stream = client
        .chat_stream(
            ChatRequest {
                messages: vec![ChatMessage::user("hi")],
                max_tokens: Some(5),
                temperature: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let mut output = String::new();
    while let Some(item) = stream.next().await {
        output.push_str(&item.unwrap());
    }

    assert_eq!(output, "ABC");

    handle.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn http_provider_chat_stream_supports_sse() {
    let handler = move |req: Request<Body>| async move {
        assert_eq!(req.method(), hyper::Method::POST);
        assert_eq!(req.uri().path(), "/complete");

        let bytes = hyper::body::to_bytes(req.into_body())
            .await
            .expect("read request body");
        let json: Value = serde_json::from_slice(&bytes).expect("parse json");
        assert_eq!(json["stream"], true);
        assert!(
            json["prompt"]
                .as_str()
                .unwrap_or_default()
                .contains("User:"),
            "expected prompt to contain the formatted chat messages"
        );

        let chunks = vec![
            Ok::<_, std::io::Error>(hyper::body::Bytes::from(
                "data: {\"completion\":\"Hello\"}\n\n",
            )),
            Ok::<_, std::io::Error>(hyper::body::Bytes::from(
                "data: {\"completion\":\" world\"}\n\n",
            )),
            Ok::<_, std::io::Error>(hyper::body::Bytes::from("data: [DONE]\n\n")),
        ];

        Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::wrap_stream(futures::stream::iter(chunks)))
            .unwrap()
    };

    let (addr, handle) = spawn_server(handler);
    let mut cfg = base_config(
        AiProviderKind::Http,
        Url::parse(&format!("http://{addr}/complete")).unwrap(),
        "test-model",
    );
    cfg.api_key = Some("test-key".to_string());

    let client = AiClient::from_config(&cfg).unwrap();
    let mut stream = client
        .chat_stream(
            ChatRequest {
                messages: vec![ChatMessage::user("hi")],
                max_tokens: Some(5),
                temperature: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let mut parts = Vec::new();
    while let Some(item) = stream.next().await {
        parts.push(item.unwrap());
    }
    assert_eq!(parts, vec!["Hello".to_string(), " world".to_string()]);
    assert_eq!(parts.concat(), "Hello world");

    handle.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn http_provider_chat_stream_timeout_is_idle_based_not_total_duration() {
    let handler = move |req: Request<Body>| async move {
        if req.uri().path() != "/complete" {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())
                .unwrap();
        }

        let bytes = hyper::body::to_bytes(req.into_body())
            .await
            .expect("read request body");
        let json: Value = serde_json::from_slice(&bytes).expect("parse json");
        assert_eq!(json["stream"], true);

        // Send chunks frequently enough to stay under the provider idle timeout,
        // but allow the total stream duration to exceed it.
        let body_stream = async_stream::stream! {
            let parts = [
                "data: {\"completion\":\"A\"}\n\n",
                "data: {\"completion\":\"B\"}\n\n",
                "data: {\"completion\":\"C\"}\n\n",
                "data: [DONE]\n\n",
            ];

            for (idx, part) in parts.into_iter().enumerate() {
                if idx != 0 {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                yield Ok::<_, std::io::Error>(hyper::body::Bytes::from(part));
            }
        };

        Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::wrap_stream(body_stream))
            .unwrap()
    };

    let (addr, handle) = spawn_server(handler);
    let mut cfg = base_config(
        AiProviderKind::Http,
        Url::parse(&format!("http://{addr}/complete")).unwrap(),
        "test-model",
    );
    cfg.provider.timeout_ms = 100;

    let client = AiClient::from_config(&cfg).unwrap();
    let mut stream = client
        .chat_stream(
            ChatRequest {
                messages: vec![ChatMessage::user("hi")],
                max_tokens: Some(5),
                temperature: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let mut output = String::new();
    while let Some(item) = stream.next().await {
        output.push_str(&item.unwrap());
    }

    assert_eq!(output, "ABC");

    handle.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn http_provider_chat_stream_falls_back_to_json() {
    let handler = move |req: Request<Body>| async move {
        if req.uri().path() != "/complete" {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())
                .unwrap();
        }

        Response::builder()
            .header("content-type", "application/json")
            .body(Body::from(r#"{"completion":"Hello"}"#))
            .unwrap()
    };

    let (addr, handle) = spawn_server(handler);
    let mut cfg = base_config(
        AiProviderKind::Http,
        Url::parse(&format!("http://{addr}/complete")).unwrap(),
        "test-model",
    );
    cfg.api_key = Some("test-key".to_string());

    let client = AiClient::from_config(&cfg).unwrap();
    let mut stream = client
        .chat_stream(
            ChatRequest {
                messages: vec![ChatMessage::user("hi")],
                max_tokens: Some(5),
                temperature: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let first = stream.next().await.expect("expected one chunk").unwrap();
    assert_eq!(first, "Hello");
    assert!(stream.next().await.is_none(), "expected stream to end");

    handle.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn azure_openai_chat_stream_cancellation_interrupts_waiting_for_next_chunk() {
    let handler = move |req: Request<Body>| async move {
        if req.uri().path() != "/openai/deployments/my-deployment/chat/completions" {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())
                .unwrap();
        }

        let body_stream = async_stream::stream! {
            yield Ok::<_, std::io::Error>(hyper::body::Bytes::from(
                "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n",
            ));
            tokio::time::sleep(Duration::from_secs(2)).await;
            yield Ok::<_, std::io::Error>(hyper::body::Bytes::from(
                "data: {\"choices\":[{\"delta\":{\"content\":\" never\"}}]}\n\n",
            ));
        };

        Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::wrap_stream(body_stream))
            .unwrap()
    };

    let (addr, handle) = spawn_server(handler);
    let mut cfg = base_config(
        AiProviderKind::AzureOpenAi,
        Url::parse(&format!("http://{addr}")).unwrap(),
        "unused",
    );
    cfg.api_key = Some("test-key".to_string());
    cfg.provider.azure_deployment = Some("my-deployment".to_string());
    cfg.provider.azure_api_version = Some("2024-02-01".to_string());
    cfg.provider.timeout_ms = 500;

    let client = AiClient::from_config(&cfg).unwrap();

    let cancel = CancellationToken::new();
    let mut stream = client
        .chat_stream(
            ChatRequest {
                messages: vec![ChatMessage::user("hi")],
                max_tokens: Some(5),
                temperature: None,
            },
            cancel.clone(),
        )
        .await
        .unwrap();

    let first = stream.next().await.expect("first chunk").unwrap();
    assert_eq!(first, "Hello");

    cancel.cancel();

    let next = tokio::time::timeout(Duration::from_millis(200), stream.next())
        .await
        .expect("stream should notice cancellation quickly")
        .expect("expected cancellation error");
    assert!(matches!(next, Err(AiError::Cancelled)));

    handle.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn azure_openai_chat_stream_times_out_when_server_stalls() {
    let handler = move |req: Request<Body>| async move {
        if req.uri().path() != "/openai/deployments/my-deployment/chat/completions" {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())
                .unwrap();
        }

        let body_stream = async_stream::stream! {
            yield Ok::<_, std::io::Error>(hyper::body::Bytes::from(
                "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n",
            ));
            tokio::time::sleep(Duration::from_millis(500)).await;
            yield Ok::<_, std::io::Error>(hyper::body::Bytes::from("data: [DONE]\n\n"));
        };

        Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::wrap_stream(body_stream))
            .unwrap()
    };

    let (addr, handle) = spawn_server(handler);
    let mut cfg = base_config(
        AiProviderKind::AzureOpenAi,
        Url::parse(&format!("http://{addr}")).unwrap(),
        "unused",
    );
    cfg.api_key = Some("test-key".to_string());
    cfg.provider.azure_deployment = Some("my-deployment".to_string());
    cfg.provider.azure_api_version = Some("2024-02-01".to_string());
    cfg.provider.timeout_ms = 100;

    let client = AiClient::from_config(&cfg).unwrap();

    let mut stream = client
        .chat_stream(
            ChatRequest {
                messages: vec![ChatMessage::user("hi")],
                max_tokens: Some(5),
                temperature: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let first = stream.next().await.expect("first chunk").unwrap();
    assert_eq!(first, "Hello");

    let next = stream.next().await.expect("expected timeout error");
    assert!(matches!(next, Err(AiError::Timeout)));

    handle.abort();
}
