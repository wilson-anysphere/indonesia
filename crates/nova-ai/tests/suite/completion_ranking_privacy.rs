use hyper::{
    service::{make_service_fn, service_fn},
    Body, Request, Response, Server,
};
use nova_ai::{AiClient, CompletionRanker, LlmCompletionRanker};
use nova_config::{AiConfig, AiProviderKind, AiPrivacyConfig};
use serde_json::Value;
use std::{convert::Infallible, net::SocketAddr, sync::Arc};
use std::time::Duration;
use tokio::task::JoinHandle;
use url::Url;

use nova_core::{CompletionContext, CompletionItem, CompletionItemKind};

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
    cfg.provider.model = "default".to_string();
    cfg.provider.max_tokens = 64;
    cfg.provider.timeout_ms = 1_000;
    cfg.provider.concurrency = Some(1);
    cfg.privacy = AiPrivacyConfig {
        local_only: false,
        anonymize_identifiers: Some(true),
        // Regression coverage: identifier anonymization should be sufficient to prevent leaks.
        // If completion candidates ever get serialized as JSON strings (inside fences) or moved
        // outside fenced blocks, disabling string-literal redaction would leak raw identifiers.
        redact_sensitive_strings: Some(false),
        redact_numeric_literals: Some(false),
        strip_or_redact_comments: Some(false),
        ..AiPrivacyConfig::default()
    };
    cfg.cache_enabled = false;
    cfg
}

#[tokio::test]
async fn completion_ranking_prompt_does_not_leak_identifiers_when_anonymized() {
    let captured_prompt: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));
    let captured_prompt_for_handler = captured_prompt.clone();

    let handler = move |req: Request<Body>| {
        let captured_prompt = captured_prompt_for_handler.clone();
        async move {
            assert_eq!(req.method(), hyper::Method::POST);
            assert_eq!(req.uri().path(), "/complete");

            let bytes = hyper::body::to_bytes(req.into_body()).await.unwrap();
            let json: Value = serde_json::from_slice(&bytes).unwrap();
            let prompt = json
                .get("prompt")
                .and_then(|v| v.as_str())
                .expect("prompt field should be string")
                .to_string();
            *captured_prompt.lock().expect("prompt mutex") = Some(prompt);

            Response::new(Body::from(r#"{"completion":"ok"}"#))
        }
    };

    let (addr, handle) = spawn_server(handler);
    let cfg = http_config(Url::parse(&format!("http://{addr}/complete")).unwrap());
    let client = Arc::new(AiClient::from_config(&cfg).expect("AiClient builds"));
    // The default ranker timeout is tuned for interactive LSP requests. Increase it for this
    // regression test so we reliably capture the outgoing prompt, even when the test binary is
    // under load (e.g. when running with additional feature flags).
    let ranker = LlmCompletionRanker::new(client).with_timeout(Duration::from_secs(2));

    let ctx = CompletionContext::new(
        "my",
        "SecretService svc = new SecretService();\nsvc.",
    );
    let candidates = vec![
        CompletionItem::new("mySecretMethod", CompletionItemKind::Method),
        // Keep both candidates the same kind so the prompt still needs distinct (anonymized) labels
        // to be useful. If labels were dropped or JSON-quoted, the model would lose signal and/or
        // identifiers could leak.
        CompletionItem::new("mySecretField", CompletionItemKind::Method),
    ];

    // We don't care about the returned ordering for this regression test, only that the request is
    // sent with a sanitized prompt.
    let _ = ranker.rank_completions(&ctx, candidates).await;

    let prompt = captured_prompt
        .lock()
        .expect("prompt mutex")
        .clone()
        .expect("expected prompt to be captured");

    let candidates_marker = "Candidates:\n```";
    let candidates_start = prompt
        .find(candidates_marker)
        .expect("expected candidates section marker in prompt");
    let after_marker = &prompt[candidates_start + candidates_marker.len()..];
    let fence_line_end = after_marker
        .find('\n')
        .expect("expected candidates code fence to end with newline");
    let after_fence = &after_marker[fence_line_end + 1..];
    let candidates_end = after_fence
        .find("```")
        .expect("expected closing candidates code fence");
    let candidates_block = &after_fence[..candidates_end];

    // Raw identifiers should not be visible to the provider.
    for leaked in ["SecretService", "mySecretMethod", "mySecretField"] {
        assert!(
            !prompt.contains(leaked),
            "expected provider prompt to not contain raw identifier {leaked:?}\n{prompt}"
        );
    }

    // Numeric candidate IDs must remain intact so the local caller can map the model's output.
    let line0 = candidates_block
        .lines()
        .find(|line| line.starts_with("0:"))
        .unwrap_or_else(|| {
            panic!(
                "expected candidates block to contain a line starting with \"0:\"\n{candidates_block}"
            )
        });
    let line1 = candidates_block
        .lines()
        .find(|line| line.starts_with("1:"))
        .unwrap_or_else(|| {
            panic!(
                "expected candidates block to contain a line starting with \"1:\"\n{candidates_block}"
            )
        });

    for (idx, line) in [line0, line1].into_iter().enumerate() {
        assert!(
            line.contains("id_"),
            "expected candidate label to be anonymized (contain id_*) on line (idx={idx}): {line:?}\n{candidates_block}"
        );
        assert!(
            !line.contains('"'),
            "candidate labels must not be quoted as JSON strings inside the code fence (would bypass identifier anonymization)\nline={line:?}\n{candidates_block}"
        );
    }

    let rest0 = line0.splitn(2, ':').nth(1).unwrap_or("").trim();
    let rest1 = line1.splitn(2, ':').nth(1).unwrap_or("").trim();
    assert_ne!(
        rest0,
        rest1,
        "expected candidate lines to differ after anonymization (labels should remain distinguishable)\n{candidates_block}"
    );

    // Ensure we actually anonymized (rather than dropping the candidate list entirely).
    assert!(
        candidates_block.contains("id_"),
        "expected anonymized placeholder to appear in candidates block\n{candidates_block}"
    );

    handle.abort();
}
