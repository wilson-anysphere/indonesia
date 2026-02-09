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

fn openai_compatible_config(url: Url) -> AiConfig {
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
        privacy: AiPrivacyConfig::default(),
        enabled: true,
        ..AiConfig::default()
    }
}

#[tokio::test]
async fn code_review_prompt_requests_structured_markdown_output() {
    let handler = move |req: Request<Body>| async move {
        assert_eq!(req.uri().path(), "/v1/chat/completions");

        let bytes = hyper::body::to_bytes(req.into_body())
            .await
            .expect("read body");
        let json: Value = serde_json::from_slice(&bytes).expect("parse json");

        let user_prompt = json["messages"][1]["content"]
            .as_str()
            .expect("user prompt is string");

        // Structure headings
        assert!(user_prompt.contains("## Summary"), "{user_prompt}");
        assert!(
            user_prompt.contains("## Issues & Suggestions"),
            "{user_prompt}"
        );
        assert!(user_prompt.contains("## Tests"), "{user_prompt}");

        // Grouping instructions
        assert!(user_prompt.contains("### path/to/File.java"), "{user_prompt}");
        for category in [
            "Correctness",
            "Performance",
            "Security",
            "Tests",
            "Maintainability",
        ] {
            assert!(
                user_prompt.contains(category),
                "missing category {category} in prompt: {user_prompt}"
            );
        }

        // Severity labels
        for severity in ["BLOCKER", "MAJOR", "MINOR"] {
            assert!(
                user_prompt.contains(severity),
                "missing severity {severity} in prompt: {user_prompt}"
            );
        }

        // Plain Markdown requirement (no structured JSON output).
        assert!(user_prompt.contains("plain Markdown"), "{user_prompt}");
        assert!(user_prompt.contains("no JSON"), "{user_prompt}");

        // Expected per-issue fields.
        for field in ["Where:", "Why it matters:", "Suggestion:"] {
            assert!(
                user_prompt.contains(field),
                "missing expected issue field {field} in prompt: {user_prompt}"
            );
        }

        // Ensure we call out potential omissions due to excluded_paths.
        assert!(user_prompt.contains("excluded_paths"), "{user_prompt}");
        // Ensure we also call out potential truncation.
        assert!(user_prompt.contains("truncated"), "{user_prompt}");

        // The omission placeholder string should only appear when the diff is actually omitted,
        // not as part of the general prompt instructions.
        assert!(
            !user_prompt.contains("[diff omitted due to excluded_paths]"),
            "{user_prompt}"
        );

        Response::new(Body::from(r#"{"choices":[{"message":{"content":"ok"}}]}"#))
    };

    let (addr, handle) = spawn_server(handler);
    let url = Url::parse(&format!("http://{addr}")).unwrap();
    let ai = NovaAi::new(&openai_compatible_config(url)).unwrap();

    let diff = "\
diff --git a/src/Main.java b/src/Main.java
index 0000000..1111111 100644
--- a/src/Main.java
+++ b/src/Main.java
@@ -1,3 +1,3 @@
-class Main { }
+class Main { int x = 1; }
";

    let out = ai
        .code_review(diff, CancellationToken::new())
        .await
        .expect("code review request succeeds");
    assert_eq!(out, "ok");

    handle.abort();
}
