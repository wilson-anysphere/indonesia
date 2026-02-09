use hyper::{
    service::{make_service_fn, service_fn},
    Body, Request, Response, Server,
};
use nova_ai::{AiClient, ChatMessage, ChatRequest};
use nova_config::{AiConfig, AiProviderKind, AiPrivacyConfig};
use serde_json::Value;
use std::{convert::Infallible, net::SocketAddr, sync::Arc};
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
    cfg.provider.model = "default".to_string();
    cfg.provider.max_tokens = 64;
    cfg.provider.timeout_ms = 1_000;
    cfg.provider.concurrency = Some(1);
    cfg.privacy = AiPrivacyConfig {
        local_only: false,
        anonymize_identifiers: Some(true),
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
    let client = AiClient::from_config(&cfg).expect("AiClient builds");

    // A completion-ranking prompt that includes obviously sensitive identifiers *inside fenced
    // blocks* so AiClient's privacy filter can anonymize them. Candidate labels are deliberately
    // unquoted: if they were JSON strings in a fenced block, the anonymizer would treat them as
    // string literals and leak raw identifiers.
    let ranking_prompt = r#"You are Nova, a Java completion ranking engine.

Code context:
```java
class SecretService {
  void run() {
    mySecretMethod();
  }
}
```

Candidates (candidate_id label):
```text
101 mySecretMethod
102 mySecretField
```

Return JSON only: {"best_candidate_id": 0}
"#;

    client
        .chat(
            ChatRequest {
                messages: vec![ChatMessage::user(ranking_prompt)],
                max_tokens: Some(16),
                temperature: Some(0.0),
            },
            CancellationToken::new(),
        )
        .await
        .expect("chat succeeds");

    let prompt = captured_prompt
        .lock()
        .expect("prompt mutex")
        .clone()
        .expect("expected prompt to be captured");

    // Raw identifiers should not be visible to the provider.
    for leaked in ["SecretService", "mySecretMethod", "mySecretField"] {
        assert!(
            !prompt.contains(leaked),
            "expected provider prompt to not contain raw identifier {leaked:?}\n{prompt}"
        );
    }

    // Numeric candidate IDs must remain intact so the local caller can map the model's output.
    for id in ["101", "102"] {
        assert!(
            prompt.contains(id),
            "expected provider prompt to contain numeric candidate id {id:?}\n{prompt}"
        );
    }

    // Ensure we actually anonymized (rather than dropping the candidate list entirely).
    assert!(
        prompt.contains("id_0"),
        "expected anonymized placeholder to appear in provider prompt\n{prompt}"
    );

    handle.abort();
}

