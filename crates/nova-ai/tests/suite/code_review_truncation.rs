use hyper::{
    service::{make_service_fn, service_fn},
    Body, Request, Response, Server,
};
use nova_ai::NovaAi;
use nova_config::{AiConfig, AiPrivacyConfig, AiProviderConfig, AiProviderKind};
use serde_json::Value;
use std::{convert::Infallible, net::SocketAddr};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use url::Url;

fn extract_diff_block(prompt: &str) -> Option<&str> {
    let (_, rest) = prompt.split_once("```diff\n")?;
    let (diff, _) = rest.split_once("\n```")?;
    Some(diff)
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

fn base_config(url: Url) -> AiConfig {
    AiConfig {
        provider: AiProviderConfig {
            kind: AiProviderKind::OpenAiCompatible,
            url,
            model: "test-model".to_string(),
            max_tokens: 128,
            timeout_ms: 1_000,
            concurrency: Some(1),
            ..AiProviderConfig::default()
        },
        privacy: AiPrivacyConfig {
            local_only: true,
            anonymize_identifiers: Some(false),
            ..AiPrivacyConfig::default()
        },
        enabled: true,
        ..AiConfig::default()
    }
}

fn cloud_config(url: Url) -> AiConfig {
    AiConfig {
        provider: AiProviderConfig {
            kind: AiProviderKind::OpenAiCompatible,
            url,
            model: "test-model".to_string(),
            max_tokens: 128,
            timeout_ms: 1_000,
            concurrency: Some(1),
            ..AiProviderConfig::default()
        },
        privacy: AiPrivacyConfig {
            // Enable cloud-mode defaults (identifier anonymization + redaction).
            local_only: false,
            anonymize_identifiers: None,
            ..AiPrivacyConfig::default()
        },
        enabled: true,
        ..AiConfig::default()
    }
}

#[tokio::test]
async fn code_review_truncates_large_diffs_before_sending() {
    let limit = 200usize;

    let header = "diff --git a/src/Main.java b/src/Main.java\n";
    let tail = "TAIL_MARKER_9876543210\n";
    let diff = format!("{header}{}{tail}", "A".repeat(1_000));

    let handler = move |req: Request<Body>| async move {
        assert_eq!(req.uri().path(), "/v1/chat/completions");
        let bytes = hyper::body::to_bytes(req.into_body())
            .await
            .expect("body bytes");
        let json: Value = serde_json::from_slice(&bytes).expect("json body");

        let user = json["messages"][1]["content"]
            .as_str()
            .expect("messages[1].content string");
        let diff_part = extract_diff_block(user).expect("diff fenced block present");
        assert!(
            diff_part.contains("[diff truncated: omitted "),
            "expected truncation marker in diff block; got: {diff_part}"
        );

        assert!(
            diff_part.starts_with(header),
            "expected diff to keep beginning; got: {diff_part}"
        );
        assert!(
            diff_part.ends_with(tail),
            "expected diff to keep end; got: {diff_part}"
        );
        assert!(
            diff_part.chars().count() <= limit,
            "expected diff part <= {limit} chars; got {} chars",
            diff_part.chars().count()
        );

        Response::new(Body::from(r#"{"choices":[{"message":{"content":"ok"}}]}"#))
    };

    let (addr, handle) = spawn_server(handler);

    let mut cfg = base_config(Url::parse(&format!("http://{addr}")).unwrap());
    cfg.features.code_review_max_diff_chars = limit;

    let ai = NovaAi::new(&cfg).expect("NovaAi builds");
    let out = ai
        .code_review(&diff, CancellationToken::new())
        .await
        .expect("code_review succeeds");
    assert_eq!(out, "ok");

    handle.abort();
}

#[tokio::test]
async fn code_review_does_not_change_small_diffs() {
    let limit = 1_000usize;
    let diff = "diff --git a/src/Main.java b/src/Main.java\n+class Main {}\n";

    let handler = move |req: Request<Body>| async move {
        assert_eq!(req.uri().path(), "/v1/chat/completions");
        let bytes = hyper::body::to_bytes(req.into_body())
            .await
            .expect("body bytes");
        let json: Value = serde_json::from_slice(&bytes).expect("json body");

        let user = json["messages"][1]["content"]
            .as_str()
            .expect("messages[1].content string");

        let diff_part = extract_diff_block(user).expect("diff fenced block present");
        assert!(
            !diff_part.contains("[diff truncated: omitted "),
            "did not expect truncation marker in diff block for small diff; got: {diff_part}"
        );

        assert_eq!(diff_part, diff);

        Response::new(Body::from(r#"{"choices":[{"message":{"content":"ok"}}]}"#))
    };

    let (addr, handle) = spawn_server(handler);

    let mut cfg = base_config(Url::parse(&format!("http://{addr}")).unwrap());
    cfg.features.code_review_max_diff_chars = limit;

    let ai = NovaAi::new(&cfg).expect("NovaAi builds");
    let out = ai
        .code_review(diff, CancellationToken::new())
        .await
        .expect("code_review succeeds");
    assert_eq!(out, "ok");

    handle.abort();
}

#[tokio::test]
async fn code_review_truncation_marker_survives_identifier_anonymization() {
    let limit = 200usize;
    let diff = format!(
        "diff --git a/src/Main.java b/src/Main.java\n\
\"HEAD_MARKER\"\n\
{}\n\
\"TAIL_MARKER\"\n",
        "~".repeat(2_000)
    );

    let handler = move |req: Request<Body>| async move {
        assert_eq!(req.uri().path(), "/v1/chat/completions");
        let bytes = hyper::body::to_bytes(req.into_body())
            .await
            .expect("body bytes");
        let json: Value = serde_json::from_slice(&bytes).expect("json body");

        let user = json["messages"][1]["content"]
            .as_str()
            .expect("messages[1].content string");
        let diff_part = extract_diff_block(user).expect("diff fenced block present");
        assert!(
            diff_part.contains("[diff truncated: omitted "),
            "expected truncation marker to survive anonymization; got: {diff_part}"
        );
        assert!(
            diff_part.contains("\"HEAD_MARKER\""),
            "expected head marker to remain in diff block: {diff_part}"
        );
        assert!(
            diff_part.contains("\"TAIL_MARKER\""),
            "expected tail marker to remain in diff block: {diff_part}"
        );
        assert!(
            diff_part.chars().count() <= limit,
            "expected diff part <= {limit} chars; got {} chars",
            diff_part.chars().count()
        );

        Response::new(Body::from(r#"{"choices":[{"message":{"content":"ok"}}]}"#))
    };

    let (addr, handle) = spawn_server(handler);

    let mut cfg = cloud_config(Url::parse(&format!("http://{addr}")).unwrap());
    cfg.features.code_review_max_diff_chars = limit;

    let ai = NovaAi::new(&cfg).expect("NovaAi builds");
    let out = ai
        .code_review(&diff, CancellationToken::new())
        .await
        .expect("code_review succeeds");
    assert_eq!(out, "ok");

    handle.abort();
}
