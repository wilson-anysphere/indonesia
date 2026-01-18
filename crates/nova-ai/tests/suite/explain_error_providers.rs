use hyper::{
    service::{make_service_fn, service_fn},
    Body, Request, Response, Server,
};
use nova_ai::{ContextRequest, NovaAi, PrivacyMode};
use nova_config::{AiConfig, AiPrivacyConfig, AiProviderConfig, AiProviderKind};
use serde_json::Value;
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
    let local_only = matches!(
        kind,
        AiProviderKind::Ollama | AiProviderKind::OpenAiCompatible
    );
    AiConfig {
        provider: AiProviderConfig {
            kind: kind.clone(),
            url,
            model: model.to_string(),
            max_tokens: 128,
            timeout_ms: 1_000,
            concurrency: Some(1),
            ..AiProviderConfig::default()
        },
        privacy: AiPrivacyConfig {
            local_only,
            anonymize_identifiers: Some(false),
            ..AiPrivacyConfig::default()
        },
        enabled: true,
        ..AiConfig::default()
    }
}

fn dummy_ctx() -> ContextRequest {
    ContextRequest {
        file_path: None,
        focal_code: "class A { void m(){ x(); } }".to_string(),
        enclosing_context: None,
        project_context: None,
        semantic_context: None,
        related_symbols: Vec::new(),
        related_code: Vec::new(),
        cursor: None,
        diagnostics: Vec::new(),
        extra_files: Vec::new(),
        doc_comments: None,
        include_doc_comments: false,
        token_budget: 200,
        privacy: PrivacyMode::default(),
    }
}

#[tokio::test]
async fn explain_error_works_for_each_provider_kind() {
    // Ollama
    {
        let handler = move |req: Request<Body>| async move {
            assert_eq!(req.uri().path(), "/api/chat");
            let bytes = hyper::body::to_bytes(req.into_body()).await.unwrap();
            let json: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(json["model"], "llama3");
            Response::new(Body::from(
                r#"{"message":{"content":"explanation"},"done":true}"#,
            ))
        };
        let (addr, handle) = spawn_server(handler);
        let mut cfg = base_config(
            AiProviderKind::Ollama,
            Url::parse(&format!("http://{addr}")).unwrap(),
            "llama3",
        );
        cfg.privacy.local_only = true;
        let ai = NovaAi::new(&cfg).unwrap();
        let out = ai
            .explain_error("cannot find symbol", dummy_ctx(), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(out, "explanation");
        handle.abort();
    }

    // OpenAI-compatible
    {
        let handler = move |req: Request<Body>| async move {
            assert_eq!(req.uri().path(), "/v1/chat/completions");
            let bytes = hyper::body::to_bytes(req.into_body()).await.unwrap();
            let json: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(json["model"], "test-model");
            Response::new(Body::from(
                r#"{"choices":[{"message":{"content":"explanation"}}]}"#,
            ))
        };
        let (addr, handle) = spawn_server(handler);
        let cfg = base_config(
            AiProviderKind::OpenAiCompatible,
            Url::parse(&format!("http://{addr}")).unwrap(),
            "test-model",
        );
        let ai = NovaAi::new(&cfg).unwrap();
        let out = ai
            .explain_error("cannot find symbol", dummy_ctx(), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(out, "explanation");
        handle.abort();
    }

    // OpenAI (cloud)
    {
        let handler = move |req: Request<Body>| async move {
            assert_eq!(req.uri().path(), "/v1/chat/completions");
            assert_eq!(
                req.headers()
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap(),
                "Bearer test-key"
            );
            Response::new(Body::from(
                r#"{"choices":[{"message":{"content":"explanation"}}]}"#,
            ))
        };
        let (addr, handle) = spawn_server(handler);
        let mut cfg = base_config(
            AiProviderKind::OpenAi,
            Url::parse(&format!("http://{addr}")).unwrap(),
            "gpt-4o-mini",
        );
        cfg.api_key = Some("test-key".to_string());
        cfg.privacy.local_only = false;
        let ai = NovaAi::new(&cfg).unwrap();
        let out = ai
            .explain_error("cannot find symbol", dummy_ctx(), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(out, "explanation");
        handle.abort();
    }

    // Anthropic
    {
        let handler = move |req: Request<Body>| async move {
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
            Response::new(Body::from(r#"{"content":[{"text":"explanation"}]}"#))
        };
        let (addr, handle) = spawn_server(handler);
        let mut cfg = base_config(
            AiProviderKind::Anthropic,
            Url::parse(&format!("http://{addr}")).unwrap(),
            "claude-3-5-sonnet-latest",
        );
        cfg.api_key = Some("test-key".to_string());
        cfg.privacy.local_only = false;
        let ai = NovaAi::new(&cfg).unwrap();
        let out = ai
            .explain_error("cannot find symbol", dummy_ctx(), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(out, "explanation");
        handle.abort();
    }

    // Gemini
    {
        let handler = move |req: Request<Body>| async move {
            assert_eq!(
                req.uri().path(),
                "/v1beta/models/gemini-1.5-flash:generateContent"
            );
            assert!(req
                .uri()
                .query()
                .expect("expected query string to be present")
                .contains("key=test-key"));
            Response::new(Body::from(
                r#"{"candidates":[{"content":{"parts":[{"text":"explanation"}]}}]}"#,
            ))
        };
        let (addr, handle) = spawn_server(handler);
        let mut cfg = base_config(
            AiProviderKind::Gemini,
            Url::parse(&format!("http://{addr}")).unwrap(),
            "gemini-1.5-flash",
        );
        cfg.api_key = Some("test-key".to_string());
        cfg.privacy.local_only = false;
        let ai = NovaAi::new(&cfg).unwrap();
        let out = ai
            .explain_error("cannot find symbol", dummy_ctx(), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(out, "explanation");
        handle.abort();
    }

    // Azure OpenAI
    {
        let handler = move |req: Request<Body>| async move {
            assert_eq!(
                req.uri().path(),
                "/openai/deployments/my-deployment/chat/completions"
            );
            assert!(req
                .uri()
                .query()
                .expect("expected query string to be present")
                .contains("api-version=2024-02-01"));
            assert_eq!(
                req.headers()
                    .get("api-key")
                    .and_then(|v| v.to_str().ok())
                    .unwrap(),
                "test-key"
            );
            Response::new(Body::from(
                r#"{"choices":[{"message":{"content":"explanation"}}]}"#,
            ))
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
        cfg.privacy.local_only = false;
        let ai = NovaAi::new(&cfg).unwrap();
        let out = ai
            .explain_error("cannot find symbol", dummy_ctx(), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(out, "explanation");
        handle.abort();
    }

    // HTTP JSON
    {
        let handler = move |req: Request<Body>| async move {
            assert_eq!(req.uri().path(), "/complete");
            assert_eq!(
                req.headers()
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap(),
                "Bearer test-key"
            );
            Response::new(Body::from(r#"{"completion":"explanation"}"#))
        };
        let (addr, handle) = spawn_server(handler);
        let mut cfg = base_config(
            AiProviderKind::Http,
            Url::parse(&format!("http://{addr}/complete")).unwrap(),
            "default",
        );
        cfg.api_key = Some("test-key".to_string());
        cfg.privacy.local_only = false;
        let ai = NovaAi::new(&cfg).unwrap();
        let out = ai
            .explain_error("cannot find symbol", dummy_ctx(), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(out, "explanation");
        handle.abort();
    }
}
