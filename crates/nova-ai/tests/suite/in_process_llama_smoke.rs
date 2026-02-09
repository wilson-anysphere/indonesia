#![cfg(feature = "local-llm")]

use nova_ai::{AiClient, AiError, ChatMessage, ChatRequest};
use nova_config::{AiConfig, AiProviderKind, InProcessLlamaConfig};
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

#[tokio::test(flavor = "current_thread")]
async fn in_process_llama_smoke_test() {
    let model_path = std::env::var("NOVA_TEST_GGUF_MODEL")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("testdata/tiny.gguf"));

    let mut cfg = AiConfig::default();
    cfg.enabled = true;
    cfg.provider.kind = AiProviderKind::InProcessLlama;
    cfg.provider.max_tokens = 32;
    cfg.provider.concurrency = None;
    cfg.provider.in_process_llama = Some(InProcessLlamaConfig {
        model_path: model_path.clone(),
        context_size: 1024,
        threads: Some(1),
        temperature: 0.2,
        top_p: 0.95,
        gpu_layers: 0,
    });

    let client = AiClient::from_config(&cfg);

    if !model_path.exists() {
        let err = client.expect_err("expected missing model to error");
        assert!(
            matches!(err, AiError::InvalidConfig(msg) if msg.contains("GGUF model file not found")),
            "unexpected error: {err:?}"
        );
        return;
    }

    let client = client.expect("client should construct with existing model");
    let out = client
        .chat(
            ChatRequest {
                messages: vec![ChatMessage::user("Hello")],
                max_tokens: Some(8),
            },
            CancellationToken::new(),
        )
        .await
        .expect("chat should succeed");

    assert!(
        !out.trim().is_empty(),
        "expected non-empty output from local model"
    );
}
