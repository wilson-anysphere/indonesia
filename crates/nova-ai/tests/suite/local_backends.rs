use async_stream::stream;
use futures::stream::iter;
use futures::StreamExt;
use hyper::{
    service::{make_service_fn, service_fn},
    Body, Request, Response, Server, StatusCode,
};
use nova_ai::{AiClient, AiError, ChatMessage, ChatRequest, ContextRequest, NovaAi, PrivacyMode};
use nova_config::{AiConfig, AiPrivacyConfig, AiProviderConfig, AiProviderKind};
use serde_json::Value;
use std::{
    convert::Infallible,
    net::SocketAddr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::Notify;
use tokio::{sync::mpsc, task::JoinHandle};
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
            azure_deployment: None,
            azure_api_version: None,
            max_tokens: 128,
            temperature: None,
            timeout_ms: 500,
            retry_max_retries: 2,
            retry_initial_backoff_ms: 200,
            retry_max_backoff_ms: 2_000,
            concurrency: Some(1),
            in_process_llama: None,
        },
        privacy: AiPrivacyConfig {
            ..AiPrivacyConfig::default()
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
            azure_deployment: None,
            azure_api_version: None,
            max_tokens: 128,
            temperature: None,
            timeout_ms: 500,
            retry_max_retries: 2,
            retry_initial_backoff_ms: 200,
            retry_max_backoff_ms: 2_000,
            concurrency: Some(1),
            in_process_llama: None,
        },
        privacy: AiPrivacyConfig {
            ..AiPrivacyConfig::default()
        },
        enabled: true,
        ..AiConfig::default()
    }
}

fn http_config(url: Url) -> AiConfig {
    AiConfig {
        provider: AiProviderConfig {
            kind: AiProviderKind::Http,
            url,
            model: "test-model".to_string(),
            azure_deployment: None,
            azure_api_version: None,
            max_tokens: 128,
            temperature: None,
            timeout_ms: 500,
            retry_max_retries: 2,
            retry_initial_backoff_ms: 200,
            retry_max_backoff_ms: 2_000,
            concurrency: Some(1),
            in_process_llama: None,
        },
        privacy: AiPrivacyConfig {
            ..AiPrivacyConfig::default()
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
            assert!(
                msg.contains("local_only"),
                "error message should mention local_only"
            );
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

#[tokio::test(flavor = "current_thread")]
async fn openai_compatible_request_formatting() {
    let (body_tx, mut body_rx) = mpsc::channel::<Value>(1);

    let handler = move |req: Request<Body>| {
        let body_tx = body_tx.clone();
        async move {
            assert_eq!(req.method(), hyper::Method::POST);
            assert_eq!(req.uri().path(), "/v1/chat/completions");
            assert!(req.headers().get(hyper::header::AUTHORIZATION).is_none());

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
                temperature: None,
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

#[tokio::test(flavor = "current_thread")]
async fn openai_compatible_sends_authorization_header_when_api_key_is_set() {
    let handler = move |req: Request<Body>| async move {
        assert_eq!(req.method(), hyper::Method::POST);
        assert_eq!(req.uri().path(), "/v1/chat/completions");

        let auth = req
            .headers()
            .get(hyper::header::AUTHORIZATION)
            .expect("authorization header");
        assert_eq!(
            auth.to_str().expect("authorization header utf8"),
            "Bearer test-key"
        );

        Response::new(Body::from(
            r#"{"choices":[{"message":{"content":"hello"}}]}"#,
        ))
    };

    let (addr, handle) = spawn_server(handler);
    let url = Url::parse(&format!("http://{addr}")).unwrap();
    let mut config = openai_config(url);
    config.api_key = Some("test-key".to_string());

    let client = AiClient::from_config(&config).unwrap();
    let content = client
        .chat(
            ChatRequest {
                messages: vec![ChatMessage::user("hi")],
                max_tokens: Some(7),
                temperature: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(content, "hello");

    handle.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn openai_compatible_list_models_sends_authorization_header_when_api_key_is_set() {
    let handler = move |req: Request<Body>| async move {
        assert_eq!(req.method(), hyper::Method::GET);
        assert_eq!(req.uri().path(), "/v1/models");

        let auth = req
            .headers()
            .get(hyper::header::AUTHORIZATION)
            .expect("authorization header");
        assert_eq!(
            auth.to_str().expect("authorization header utf8"),
            "Bearer test-key"
        );

        Response::new(Body::from(r#"{"data":[{"id":"m1"},{"id":"m2"}]}"#))
    };

    let (addr, handle) = spawn_server(handler);
    let url = Url::parse(&format!("http://{addr}")).unwrap();
    let mut config = openai_config(url);
    config.api_key = Some("test-key".to_string());

    let client = AiClient::from_config(&config).unwrap();
    let models = client
        .list_models(CancellationToken::new())
        .await
        .expect("list models");

    assert_eq!(models, vec!["m1".to_string(), "m2".to_string()]);

    handle.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn openai_compatible_stream_sends_authorization_header_when_api_key_is_set() {
    let handler = move |req: Request<Body>| async move {
        assert_eq!(req.method(), hyper::Method::POST);
        assert_eq!(req.uri().path(), "/v1/chat/completions");

        let auth = req
            .headers()
            .get(hyper::header::AUTHORIZATION)
            .expect("authorization header");
        assert_eq!(
            auth.to_str().expect("authorization header utf8"),
            "Bearer test-key"
        );
        let accept = req
            .headers()
            .get(hyper::header::ACCEPT)
            .expect("accept header");
        assert_eq!(
            accept.to_str().expect("accept header utf8"),
            "text/event-stream"
        );

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
    };

    let (addr, handle) = spawn_server(handler);
    let url = Url::parse(&format!("http://{addr}")).unwrap();
    let mut config = openai_config(url);
    config.api_key = Some("test-key".to_string());

    let client = AiClient::from_config(&config).unwrap();

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
    assert_eq!(output, "Hello world");

    handle.abort();
}

#[tokio::test(flavor = "current_thread")]
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
                temperature: None,
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

#[tokio::test(flavor = "current_thread")]
async fn ollama_request_formatting_supports_base_url_with_api_suffix() {
    let handler = move |req: Request<Body>| async move {
        assert_eq!(req.method(), hyper::Method::POST);
        assert_eq!(req.uri().path(), "/api/chat");

        Response::new(Body::from(r#"{"message":{"content":"hello"},"done":true}"#))
    };

    let (addr, handle) = spawn_server(handler);
    let url = Url::parse(&format!("http://{addr}/api")).unwrap();
    let client = AiClient::from_config(&ollama_config(url)).unwrap();

    let content = client
        .chat(
            ChatRequest {
                messages: vec![ChatMessage::user("hi")],
                max_tokens: Some(11),
                temperature: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(content, "hello");

    handle.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn ollama_streaming_parsing_splits_single_json_line_across_chunks() {
    let (body_tx, mut body_rx) = mpsc::channel::<Value>(1);

    let handler = move |req: Request<Body>| {
        let body_tx = body_tx.clone();
        async move {
            if req.uri().path() != "/api/chat" {
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

            let line = br#"{"message":{"content":"Hello world"},"done":false}"#;
            let mut line_bytes = Vec::from(&line[..]);
            line_bytes.push(b'\n');
            let done = br#"{"done":true}"#;
            let mut done_bytes = Vec::from(&done[..]);
            done_bytes.push(b'\n');

            let split1 = 10;
            let split2 = 25;
            let chunks = vec![
                Ok::<_, std::io::Error>(hyper::body::Bytes::copy_from_slice(&line_bytes[..split1])),
                Ok::<_, std::io::Error>(hyper::body::Bytes::copy_from_slice(&line_bytes[split1..split2])),
                Ok::<_, std::io::Error>(hyper::body::Bytes::copy_from_slice(&line_bytes[split2..])),
                Ok::<_, std::io::Error>(hyper::body::Bytes::from(done_bytes)),
            ];

            Response::builder()
                .header("content-type", "application/x-ndjson")
                .body(Body::wrap_stream(iter(chunks)))
                .unwrap()
        }
    };

    let (addr, handle) = spawn_server(handler);
    let url = Url::parse(&format!("http://{addr}")).unwrap();
    let client = AiClient::from_config(&ollama_config(url)).unwrap();

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
    assert_eq!(output, "Hello world");

    let body = body_rx.recv().await.expect("request body");
    assert_eq!(body["stream"], true);

    handle.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn ollama_streaming_parsing_preserves_utf8_split_across_chunks() {
    let (body_tx, mut body_rx) = mpsc::channel::<Value>(1);

    let handler = move |req: Request<Body>| {
        let body_tx = body_tx.clone();
        async move {
            if req.uri().path() != "/api/chat" {
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

            let emoji = "🦀";
            let expected = format!("Hello {emoji}");
            let line = format!(
                "{{\"message\":{{\"content\":\"{expected}\"}},\"done\":false}}"
            );
            let mut line_bytes = line.into_bytes();
            line_bytes.push(b'\n');

            let emoji_bytes = emoji.as_bytes();
            let emoji_pos = line_bytes
                .windows(emoji_bytes.len())
                .position(|window| window == emoji_bytes)
                .expect("emoji should be present in response bytes");
            let split = emoji_pos + 2; // split inside the 4-byte UTF-8 sequence.

            let done = br#"{"done":true}"#;
            let mut done_bytes = Vec::from(&done[..]);
            done_bytes.push(b'\n');

            let chunks = vec![
                Ok::<_, std::io::Error>(hyper::body::Bytes::copy_from_slice(&line_bytes[..split])),
                Ok::<_, std::io::Error>(hyper::body::Bytes::copy_from_slice(&line_bytes[split..])),
                Ok::<_, std::io::Error>(hyper::body::Bytes::from(done_bytes)),
            ];

            Response::builder()
                .header("content-type", "application/x-ndjson")
                .body(Body::wrap_stream(iter(chunks)))
                .unwrap()
        }
    };

    let (addr, handle) = spawn_server(handler);
    let url = Url::parse(&format!("http://{addr}")).unwrap();
    let client = AiClient::from_config(&ollama_config(url)).unwrap();

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

    assert_eq!(output, "Hello 🦀");
    assert!(
        !output.contains('\u{FFFD}'),
        "output should not contain replacement characters: {output:?}"
    );

    let body = body_rx.recv().await.expect("request body");
    assert_eq!(body["stream"], true);

    handle.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn ollama_stream_timeout_is_idle_based_not_total_duration() {
    let handler = move |req: Request<Body>| async move {
        if req.uri().path() != "/api/chat" {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())
                .unwrap();
        }

        let _ = hyper::body::to_bytes(req.into_body())
            .await
            .expect("read request body");

        // Send chunks frequently enough to stay under the provider idle timeout,
        // but allow the total stream duration to exceed it.
        let chunks = stream! {
            let parts: [&'static [u8]; 4] = [
                br#"{"message":{"content":"A"},"done":false}
"#,
                br#"{"message":{"content":"B"},"done":false}
"#,
                br#"{"message":{"content":"C"},"done":false}
"#,
                br#"{"done":true}
"#,
            ];

            for (idx, part) in parts.into_iter().enumerate() {
                if idx != 0 {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                yield Ok::<_, std::io::Error>(hyper::body::Bytes::from_static(part));
            }
        };

        Response::builder()
            .header("content-type", "application/x-ndjson")
            .body(Body::wrap_stream(chunks))
            .unwrap()
    };

    let (addr, handle) = spawn_server(handler);
    let url = Url::parse(&format!("http://{addr}")).unwrap();

    let mut cfg = ollama_config(url);
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
            assert!(req.headers().get(hyper::header::AUTHORIZATION).is_none());
            let accept = req
                .headers()
                .get(hyper::header::ACCEPT)
                .expect("accept header");
            assert_eq!(
                accept.to_str().expect("accept header utf8"),
                "text/event-stream"
            );

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
    assert_eq!(output, "Hello world");

    let body = body_rx.recv().await.expect("request body");
    assert_eq!(body["stream"], true);

    handle.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn openai_compatible_stream_parses_sse_line_split_across_chunks() {
    let handler = move |req: Request<Body>| async move {
        if req.uri().path() != "/v1/chat/completions" {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())
                .unwrap();
        }

        let _ = hyper::body::to_bytes(req.into_body())
            .await
            .expect("read request body");

        // Split the SSE line across multiple "TCP chunks", including splitting CRLF across chunks.
        let chunks = vec![
            Ok::<_, std::io::Error>(hyper::body::Bytes::from("data: {\"choices\"")),
            Ok::<_, std::io::Error>(hyper::body::Bytes::from(
                ":[{\"delta\":{\"content\":\"Hello\"}}]}\r",
            )),
            Ok::<_, std::io::Error>(hyper::body::Bytes::from("\n\r\n")),
            Ok::<_, std::io::Error>(hyper::body::Bytes::from("data: [DONE]\r\n\r\n")),
        ];

        Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::wrap_stream(iter(chunks)))
            .unwrap()
    };

    let (addr, handle) = spawn_server(handler);
    let url = Url::parse(&format!("http://{addr}")).unwrap();
    let client = AiClient::from_config(&openai_config(url)).unwrap();

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
    assert_eq!(output, "Hello");

    handle.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn http_provider_streaming_parsing_and_request_format() {
    let (body_tx, mut body_rx) = mpsc::channel::<Value>(1);

    let handler = move |req: Request<Body>| {
        let body_tx = body_tx.clone();
        async move {
            if req.uri().path() != "/complete" {
                return Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .body(Body::empty())
                    .unwrap();
            }

            let accept = req
                .headers()
                .get(hyper::header::ACCEPT)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default();
            assert_eq!(accept, "text/event-stream");

            let bytes = hyper::body::to_bytes(req.into_body())
                .await
                .expect("read body");
            let json: Value = serde_json::from_slice(&bytes).expect("parse json");
            let _ = body_tx.send(json).await;

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
                .body(Body::wrap_stream(iter(chunks)))
                .unwrap()
        }
    };

    let (addr, handle) = spawn_server(handler);
    let url = Url::parse(&format!("http://{addr}/complete")).unwrap();
    let client = AiClient::from_config(&http_config(url)).unwrap();

    let mut stream = client
        .chat_stream(
            ChatRequest {
                messages: vec![ChatMessage::user("hi")],
                max_tokens: Some(5),
                temperature: Some(0.2),
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
    assert_eq!(body["model"], "test-model");
    assert!(body["prompt"].as_str().unwrap_or_default().contains("User:"));
    assert_eq!(body["max_tokens"], 5);
    let temp = body["temperature"]
        .as_f64()
        .expect("temperature should be a JSON number");
    assert!((temp - 0.2).abs() < 1e-6, "unexpected temperature: {temp}");

    handle.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn http_provider_streaming_falls_back_to_json_body_when_response_is_not_sse() {
    let handler = move |req: Request<Body>| async move {
        if req.uri().path() != "/complete" {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())
                .unwrap();
        }

        let accept = req
            .headers()
            .get(hyper::header::ACCEPT)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        assert_eq!(accept, "text/event-stream");

        let _ = hyper::body::to_bytes(req.into_body())
            .await
            .expect("read request body");

        Response::builder()
            .header("content-type", "application/json")
            .body(Body::from(r#"{"completion":"ok"}"#))
            .unwrap()
    };

    let (addr, handle) = spawn_server(handler);
    let url = Url::parse(&format!("http://{addr}/complete")).unwrap();
    let client = AiClient::from_config(&http_config(url)).unwrap();

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

    let mut chunks = Vec::new();
    while let Some(item) = stream.next().await {
        chunks.push(item.unwrap());
    }
    assert_eq!(chunks, vec!["ok".to_string()]);

    handle.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn http_provider_sends_authorization_header_when_api_key_is_set() {
    let handler = move |req: Request<Body>| async move {
        if req.uri().path() != "/complete" {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())
                .unwrap();
        }

        let auth = req
            .headers()
            .get(hyper::header::AUTHORIZATION)
            .expect("authorization header");
        assert_eq!(
            auth.to_str().expect("authorization header utf8"),
            "Bearer test-key"
        );

        let _ = hyper::body::to_bytes(req.into_body())
            .await
            .expect("read request body");

        Response::new(Body::from(r#"{"completion":"ok"}"#))
    };

    let (addr, handle) = spawn_server(handler);
    let url = Url::parse(&format!("http://{addr}/complete")).unwrap();
    let mut config = http_config(url);
    config.api_key = Some("test-key".to_string());

    let client = AiClient::from_config(&config).unwrap();
    let content = client
        .chat(
            ChatRequest {
                messages: vec![ChatMessage::user("hi")],
                max_tokens: Some(5),
                temperature: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(content, "ok");

    handle.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn openai_compatible_stream_parses_multibyte_utf8_split_across_chunks() {
    let handler = move |req: Request<Body>| async move {
        if req.uri().path() != "/v1/chat/completions" {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())
                .unwrap();
        }

        let _ = hyper::body::to_bytes(req.into_body())
            .await
            .expect("read request body");

        // Split the UTF-8 bytes for `世` across chunks to ensure the client buffers bytes and only
        // decodes once a full line is assembled.
        let payload = "data: {\"choices\":[{\"delta\":{\"content\":\"世\"}}]}\n\n";
        let bytes = payload.as_bytes();
        let split_at = payload.find('世').expect("payload contains multibyte char") + 1;

        let chunks = vec![
            Ok::<_, std::io::Error>(hyper::body::Bytes::from(bytes[..split_at].to_vec())),
            Ok::<_, std::io::Error>(hyper::body::Bytes::from(bytes[split_at..].to_vec())),
            Ok::<_, std::io::Error>(hyper::body::Bytes::from("data: [DONE]\n\n")),
        ];

        Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::wrap_stream(iter(chunks)))
            .unwrap()
    };

    let (addr, handle) = spawn_server(handler);
    let url = Url::parse(&format!("http://{addr}")).unwrap();
    let client = AiClient::from_config(&openai_config(url)).unwrap();

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
    assert_eq!(output, "世");

    handle.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn openai_compatible_stream_timeout_is_idle_based_not_total_duration() {
    let handler = move |req: Request<Body>| async move {
        if req.uri().path() != "/v1/chat/completions" {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())
                .unwrap();
        }

        let _ = hyper::body::to_bytes(req.into_body())
            .await
            .expect("read request body");

        // Send chunks frequently enough to stay under the provider idle timeout,
        // but allow the total stream duration to exceed it.
        let chunks = stream! {
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
            .body(Body::wrap_stream(chunks))
            .unwrap()
    };

    let (addr, handle) = spawn_server(handler);
    let url = Url::parse(&format!("http://{addr}")).unwrap();

    let mut config = openai_config(url);
    config.provider.timeout_ms = 100;
    let client = AiClient::from_config(&config).unwrap();

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
async fn openai_compatible_stream_ignores_empty_data_lines() {
    let handler = move |req: Request<Body>| async move {
        if req.uri().path() != "/v1/chat/completions" {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())
                .unwrap();
        }

        let _ = hyper::body::to_bytes(req.into_body())
            .await
            .expect("read request body");

        let chunks = vec![
            Ok::<_, std::io::Error>(hyper::body::Bytes::from("data:\n\n")),
            Ok::<_, std::io::Error>(hyper::body::Bytes::from(
                "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n",
            )),
            Ok::<_, std::io::Error>(hyper::body::Bytes::from("data: [DONE]\n\n")),
        ];

        Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::wrap_stream(iter(chunks)))
            .unwrap()
    };

    let (addr, handle) = spawn_server(handler);
    let url = Url::parse(&format!("http://{addr}")).unwrap();
    let client = AiClient::from_config(&openai_config(url)).unwrap();

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
    assert_eq!(output, "Hello");

    handle.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn openai_compatible_stream_parses_final_data_line_without_trailing_newline() {
    let handler = move |req: Request<Body>| async move {
        if req.uri().path() != "/v1/chat/completions" {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())
                .unwrap();
        }

        let _ = hyper::body::to_bytes(req.into_body())
            .await
            .expect("read request body");

        let chunks = vec![Ok::<_, std::io::Error>(hyper::body::Bytes::from(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}",
        ))];

        Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::wrap_stream(iter(chunks)))
            .unwrap()
    };

    let (addr, handle) = spawn_server(handler);
    let url = Url::parse(&format!("http://{addr}")).unwrap();
    let client = AiClient::from_config(&openai_config(url)).unwrap();

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
    assert_eq!(output, "Hello");

    handle.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn openai_compatible_stream_parses_multiline_json_across_data_fields() {
    let handler = move |req: Request<Body>| async move {
        if req.uri().path() != "/v1/chat/completions" {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())
                .unwrap();
        }

        let _ = hyper::body::to_bytes(req.into_body())
            .await
            .expect("read request body");

        // Some providers pretty-print JSON across multiple `data:` fields within a single SSE
        // event (SSE spec concatenates multiple `data:` lines with `\n`). The client should treat
        // the concatenated payload as JSON and parse it successfully.
        let chunks = vec![
            Ok::<_, std::io::Error>(hyper::body::Bytes::from("data: {\"choices\":\n")),
            Ok::<_, std::io::Error>(hyper::body::Bytes::from(
                "data: [{\"delta\":{\"content\":\"Hello\"}}]}\n\n",
            )),
            Ok::<_, std::io::Error>(hyper::body::Bytes::from("data: [DONE]\n\n")),
        ];

        Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::wrap_stream(iter(chunks)))
            .unwrap()
    };

    let (addr, handle) = spawn_server(handler);
    let url = Url::parse(&format!("http://{addr}")).unwrap();
    let client = AiClient::from_config(&openai_config(url)).unwrap();

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
    assert_eq!(output, "Hello");

    handle.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn openai_compatible_stream_times_out_when_server_stalls() {
    let handler = move |req: Request<Body>| async move {
        if req.uri().path() != "/v1/chat/completions" {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())
                .unwrap();
        }

        let _ = hyper::body::to_bytes(req.into_body())
            .await
            .expect("read request body");

        let chunks = stream! {
            yield Ok::<_, std::io::Error>(hyper::body::Bytes::from(
                "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n",
            ));

            // Stall long enough to exceed the client's idle timeout between chunks.
            tokio::time::sleep(Duration::from_millis(250)).await;

            yield Ok::<_, std::io::Error>(hyper::body::Bytes::from(
                "data: {\"choices\":[{\"delta\":{\"content\":\" world\"}}]}\n\n",
            ));
            yield Ok::<_, std::io::Error>(hyper::body::Bytes::from("data: [DONE]\n\n"));
        };

        Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::wrap_stream(chunks))
            .unwrap()
    };

    let (addr, handle) = spawn_server(handler);
    let url = Url::parse(&format!("http://{addr}")).unwrap();

    let mut config = openai_config(url);
    config.provider.timeout_ms = 100;
    let client = AiClient::from_config(&config).unwrap();

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

    let first = stream
        .next()
        .await
        .expect("first chunk")
        .expect("first chunk ok");
    assert_eq!(first, "Hello");

    let second = stream
        .next()
        .await
        .expect("timeout error")
        .expect_err("expected timeout error");
    assert!(matches!(second, AiError::Timeout));

    handle.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn openai_compatible_stream_cancellation_interrupts_waiting_for_next_chunk() {
    let handler = move |req: Request<Body>| async move {
        if req.uri().path() != "/v1/chat/completions" {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())
                .unwrap();
        }

        let _ = hyper::body::to_bytes(req.into_body())
            .await
            .expect("read request body");

        let chunks = stream! {
            yield Ok::<_, std::io::Error>(hyper::body::Bytes::from(
                "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n",
            ));

            // Hold the connection open; the client should cancel rather than waiting.
            tokio::time::sleep(Duration::from_secs(5)).await;

            yield Ok::<_, std::io::Error>(hyper::body::Bytes::from("data: [DONE]\n\n"));
        };

        Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::wrap_stream(chunks))
            .unwrap()
    };

    let (addr, handle) = spawn_server(handler);
    let url = Url::parse(&format!("http://{addr}")).unwrap();

    let mut config = openai_config(url);
    config.provider.timeout_ms = 10_000;
    let client = AiClient::from_config(&config).unwrap();

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

    let first = stream
        .next()
        .await
        .expect("first chunk")
        .expect("first chunk ok");
    assert_eq!(first, "Hello");

    cancel.cancel();

    let second = tokio::time::timeout(Duration::from_secs(1), stream.next())
        .await
        .expect("cancelled stream should return quickly")
        .expect("cancelled item")
        .expect_err("expected cancellation error");
    assert!(matches!(second, AiError::Cancelled));

    handle.abort();
}

#[tokio::test(flavor = "current_thread")]
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
                temperature: None,
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

#[tokio::test(flavor = "current_thread")]
async fn cancellation_while_waiting_for_client_semaphore() {
    let request_count = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(Notify::new());
    let (started_tx, mut started_rx) = mpsc::channel::<()>(1);

    let request_count_for_handler = request_count.clone();
    let gate_for_handler = gate.clone();
    let handler = move |req: Request<Body>| {
        let request_count = request_count_for_handler.clone();
        let gate = gate_for_handler.clone();
        let started_tx = started_tx.clone();

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

            let request_number = request_count.fetch_add(1, Ordering::SeqCst);
            if request_number == 0 {
                let _ = started_tx.send(()).await;

                // Hold the first request open so the client's concurrency semaphore is occupied.
                gate.notified().await;
            }

            Response::new(Body::from(
                r#"{"choices":[{"message":{"content":"hello"}}]}"#,
            ))
        }
    };

    let (addr, handle) = spawn_server(handler);
    let url = Url::parse(&format!("http://{addr}")).unwrap();

    let mut config = openai_config(url);
    config.provider.timeout_ms = 5_000;
    config.provider.concurrency = Some(1);
    let client = Arc::new(AiClient::from_config(&config).unwrap());

    let first = {
        let client = client.clone();
        tokio::spawn(async move {
            client
                .chat(
                    ChatRequest {
                        messages: vec![ChatMessage::user("hi")],
                        max_tokens: Some(5),
                        temperature: None,
                    },
                    CancellationToken::new(),
                )
                .await
        })
    };

    started_rx.recv().await.expect("first request started");

    let cancel = CancellationToken::new();
    let second = {
        let client = client.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            client
                .chat(
                    ChatRequest {
                        messages: vec![ChatMessage::user("hi")],
                        max_tokens: Some(5),
                        temperature: None,
                    },
                    cancel,
                )
                .await
        })
    };

    tokio::task::yield_now().await;
    cancel.cancel();

    let result = tokio::time::timeout(Duration::from_secs(1), second)
        .await
        .expect("cancelled request should not hang waiting for permit")
        .expect("join task");
    assert!(matches!(result, Err(AiError::Cancelled)));
    assert_eq!(
        request_count.load(Ordering::SeqCst),
        1,
        "cancelled request should not reach the backend"
    );

    gate.notify_one();

    let first_result = tokio::time::timeout(Duration::from_secs(1), first)
        .await
        .expect("first request should complete once gate opens")
        .expect("join task");
    assert_eq!(first_result.unwrap(), "hello");

    handle.abort();
}

#[tokio::test(flavor = "current_thread")]
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
            ContextRequest::for_java_source_range(
                "class A { void m(){ x(); } }",
                0.."class A { void m(){ x(); } }".len(),
                800,
                PrivacyMode::default(),
                true,
            ),
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
