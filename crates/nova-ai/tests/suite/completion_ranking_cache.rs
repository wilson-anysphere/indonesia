use httpmock::prelude::*;
use nova_ai::{AiClient, CompletionRanker, LlmClient, LlmCompletionRanker};
use nova_config::{AiConfig, AiProviderKind};
use nova_core::{CompletionContext, CompletionItem, CompletionItemKind};
use serde_json::json;
use std::sync::Arc;
use url::Url;

fn http_config(url: Url, model: &str) -> AiConfig {
    let mut cfg = AiConfig::default();
    cfg.enabled = true;
    cfg.provider.kind = AiProviderKind::Http;
    cfg.provider.url = url;
    cfg.provider.model = model.to_string();
    cfg.provider.timeout_ms = 1_000;
    cfg.provider.concurrency = Some(1);
    cfg.provider.max_tokens = 64;
    cfg.privacy.local_only = false;
    cfg.privacy.anonymize_identifiers = Some(false);
    cfg.cache_enabled = true;
    cfg.cache_max_entries = 32;
    cfg.cache_ttl_secs = 60;
    cfg
}

#[tokio::test]
async fn llm_completion_ranking_is_cached_for_identical_requests() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST)
            .path("/complete")
            .json_body_partial(json!({ "max_tokens": 64, "temperature": 0.0 }).to_string());
        then.status(200).json_body(json!({ "completion": "[1,0]" }));
    });

    let cfg = http_config(
        Url::parse(&format!("{}/complete", server.base_url())).unwrap(),
        "default",
    );

    let llm: Arc<dyn LlmClient> = Arc::new(AiClient::from_config(&cfg).unwrap());
    let ranker = LlmCompletionRanker::new(llm).with_max_output_tokens(64);

    let ctx = CompletionContext::new("pri", "pri");
    let items = vec![
        CompletionItem::new("print", CompletionItemKind::Method),
        CompletionItem::new("println", CompletionItemKind::Method),
    ];

    let ranked1 = ranker.rank_completions(&ctx, items.clone()).await;
    let ranked2 = ranker.rank_completions(&ctx, items).await;

    assert_eq!(ranked1, ranked2);
    assert_eq!(ranked1[0].label, "println");
    assert_eq!(ranked1[1].label, "print");

    mock.assert_hits(1);
}
