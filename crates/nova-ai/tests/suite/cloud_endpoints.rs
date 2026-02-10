use hyper::{
    service::{make_service_fn, service_fn},
    Body, Request, Response, Server,
};
use nova_ai::{AiClient, ChatMessage, ChatRequest};
use nova_config::{AiConfig, AiPrivacyConfig, AiProviderConfig, AiProviderKind};
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

fn base_config(kind: AiProviderKind, url: Url, model: &str) -> AiConfig {
    AiConfig {
        provider: AiProviderConfig {
            kind,
            url,
            model: model.to_string(),
            max_tokens: 32,
            timeout_ms: 1_000,
            concurrency: Some(1),
            ..AiProviderConfig::default()
        },
        privacy: AiPrivacyConfig {
            local_only: false,
            anonymize_identifiers: Some(false),
            ..AiPrivacyConfig::default()
        },
        enabled: true,
        ..AiConfig::default()
    }
}

async fn run_chat(client: AiClient) {
    let _ = client
        .chat(
            ChatRequest {
                messages: vec![ChatMessage::user("hello")],
                max_tokens: Some(8),
                temperature: None,
            },
            CancellationToken::new(),
        )
        .await
        .expect("chat");
}

#[tokio::test(flavor = "current_thread")]
async fn anthropic_endpoint_preserves_prefix_and_avoids_double_v1() {
    for (base_suffix, expected_path) in [
        ("anthropic", "/anthropic/v1/messages"),
        ("anthropic/", "/anthropic/v1/messages"),
        // Regression: base already includes /v1 with a trailing slash should not yield /v1/v1/messages.
        ("anthropic/v1/", "/anthropic/v1/messages"),
    ] {
        let expected_path = expected_path.to_string();
        let handler = move |req: Request<Body>| {
            let expected_path = expected_path.clone();
            async move {
                assert_eq!(req.uri().path(), expected_path);
                Response::new(Body::from(r#"{"content":[{"text":"ok"}]}"#))
            }
        };

        let (addr, handle) = spawn_server(handler);
        let url = Url::parse(&format!("http://{addr}/{base_suffix}")).expect("valid url");
        let mut cfg = base_config(
            AiProviderKind::Anthropic,
            url,
            "claude-3-5-sonnet-latest",
        );
        cfg.api_key = Some("test-key".to_string());
        let client = AiClient::from_config(&cfg).expect("client");
        run_chat(client).await;
        handle.abort();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn gemini_endpoint_preserves_prefix_and_avoids_double_v1beta() {
    for (base_suffix, expected_path) in [
        (
            "gemini",
            "/gemini/v1beta/models/gemini-1.5-flash:generateContent",
        ),
        (
            "gemini/",
            "/gemini/v1beta/models/gemini-1.5-flash:generateContent",
        ),
        // Regression: base already includes /v1beta with a trailing slash should not yield /v1beta/v1beta/...
        (
            "gemini/v1beta/",
            "/gemini/v1beta/models/gemini-1.5-flash:generateContent",
        ),
    ] {
        let expected_path = expected_path.to_string();
        let handler = move |req: Request<Body>| {
            let expected_path = expected_path.clone();
            async move {
                assert_eq!(req.uri().path(), expected_path);
                let query = req.uri().query().unwrap_or_default();
                assert!(
                    !query.contains("key="),
                    "expected Gemini api key to be sent via header, not query: {query}"
                );
                assert_eq!(
                    req.headers()
                        .get("x-goog-api-key")
                        .and_then(|v| v.to_str().ok())
                        .unwrap(),
                    "test-key"
                );
                Response::new(Body::from(
                    r#"{"candidates":[{"content":{"parts":[{"text":"ok"}]}}]}"#,
                ))
            }
        };

        let (addr, handle) = spawn_server(handler);
        let url = Url::parse(&format!("http://{addr}/{base_suffix}")).expect("valid url");
        let mut cfg = base_config(AiProviderKind::Gemini, url, "gemini-1.5-flash");
        cfg.api_key = Some("test-key".to_string());
        let client = AiClient::from_config(&cfg).expect("client");
        run_chat(client).await;
        handle.abort();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn azure_openai_endpoint_preserves_prefix_and_avoids_double_openai() {
    for (base_suffix, expected_path) in [
        (
            "azure",
            "/azure/openai/deployments/my-deployment/chat/completions",
        ),
        (
            "azure/",
            "/azure/openai/deployments/my-deployment/chat/completions",
        ),
        // Regression: base already includes /openai with a trailing slash should not yield /openai/openai/...
        (
            "azure/openai/",
            "/azure/openai/deployments/my-deployment/chat/completions",
        ),
    ] {
        let expected_path = expected_path.to_string();
        let handler = move |req: Request<Body>| {
            let expected_path = expected_path.clone();
            async move {
                assert_eq!(req.uri().path(), expected_path);
                assert!(req
                    .uri()
                    .query()
                    .unwrap_or_default()
                    .contains("api-version=2024-02-01"));
                Response::new(Body::from(
                    r#"{"choices":[{"message":{"content":"ok"}}]}"#,
                ))
            }
        };

        let (addr, handle) = spawn_server(handler);
        let url = Url::parse(&format!("http://{addr}/{base_suffix}")).expect("valid url");
        let mut cfg = base_config(AiProviderKind::AzureOpenAi, url, "unused");
        cfg.api_key = Some("test-key".to_string());
        cfg.provider.azure_deployment = Some("my-deployment".to_string());
        cfg.provider.azure_api_version = Some("2024-02-01".to_string());
        let client = AiClient::from_config(&cfg).expect("client");
        run_chat(client).await;
        handle.abort();
    }
}
