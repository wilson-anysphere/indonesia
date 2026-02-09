use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use futures::executor::block_on;
use futures::future::BoxFuture;
use nova_ai::{AiError, AiStream, ChatRequest, LlmClient};
use nova_config::AiConfig;
use nova_db::InMemoryFileStore;
use nova_ide::{completions, completions_with_ai};
use tokio_util::sync::CancellationToken;

use crate::framework_harness::{offset_to_position, CARET};

fn labels(items: &[lsp_types::CompletionItem]) -> Vec<String> {
    items.iter().map(|item| item.label.clone()).collect()
}

#[derive(Debug, Default)]
struct CountingLlmClient {
    chat_calls: AtomicUsize,
}

impl CountingLlmClient {
    fn chat_calls(&self) -> usize {
        self.chat_calls.load(Ordering::SeqCst)
    }
}

impl LlmClient for CountingLlmClient {
    fn chat<'life0, 'async_trait>(
        &'life0 self,
        _request: ChatRequest,
        _cancel: CancellationToken,
    ) -> BoxFuture<'async_trait, Result<String, AiError>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        self.chat_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { Ok("[1,0]".to_string()) })
    }

    fn chat_stream<'life0, 'async_trait>(
        &'life0 self,
        _request: ChatRequest,
        _cancel: CancellationToken,
    ) -> BoxFuture<'async_trait, Result<AiStream, AiError>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            Err(AiError::UnexpectedResponse(
                "streaming not supported in test".into(),
            ))
        })
    }

    fn list_models<'life0, 'async_trait>(
        &'life0 self,
        _cancel: CancellationToken,
    ) -> BoxFuture<'async_trait, Result<Vec<String>, AiError>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move { Ok(Vec::new()) })
    }
}

#[test]
fn completions_with_ai_skips_ranking_for_excluded_paths() {
    let text_with_caret = r#"
class Secret {
  void bar() {}

  void m() {
    int x = 0;
    <|>
  }
}
"#;

    let caret_offset = text_with_caret
        .find(CARET)
        .expect("fixture must contain <|> caret marker");
    let text = text_with_caret.replace(CARET, "");
    let position = offset_to_position(&text, caret_offset);

    let mut db = InMemoryFileStore::new();

    let excluded_path = PathBuf::from("src/secrets/Secret.java");
    let excluded_file = db.file_id_for_path(&excluded_path);
    db.set_file_text(excluded_file, text.clone());

    let included_path = PathBuf::from("src/Main.java");
    let included_file = db.file_id_for_path(&included_path);
    db.set_file_text(included_file, text.clone());

    let baseline_excluded = completions(&db, excluded_file, position);
    let baseline_included = completions(&db, included_file, position);
    assert_eq!(
        labels(&baseline_excluded),
        labels(&baseline_included),
        "expected baseline completions to be independent of file path for this fixture"
    );
    assert_eq!(
        baseline_excluded.first().map(|item| item.label.as_str()),
        Some("x"),
        "sanity check: fixture should rank in-scope locals above other items"
    );

    let mut config = AiConfig::default();
    config.enabled = true;
    config.features.completion_ranking = true;
    config.privacy.excluded_paths = vec!["src/secrets/**".to_string()];

    let llm = Arc::new(CountingLlmClient::default());

    let included_ranked = block_on(completions_with_ai(
        &db,
        included_file,
        position,
        &config,
        Some(llm.clone()),
    ));
    let calls_after_included = llm.chat_calls();
    assert!(
        calls_after_included > 0,
        "expected non-excluded file to trigger LLM completion ranking"
    );
    assert_eq!(
        included_ranked.first().map(|item| item.label.as_str()),
        Some("m"),
        "sanity check: mock LLM response should reorder the top completion item"
    );
    assert_ne!(
        labels(&included_ranked),
        labels(&baseline_included),
        "expected completion re-ranking to run for non-excluded files"
    );

    let excluded_ranked = block_on(completions_with_ai(
        &db,
        excluded_file,
        position,
        &config,
        Some(llm.clone()),
    ));
    assert_eq!(
        llm.chat_calls(),
        calls_after_included,
        "expected excluded files to bypass LLM-backed completion ranking entirely"
    );
    assert_eq!(
        labels(&excluded_ranked),
        labels(&baseline_excluded),
        "expected excluded files to bypass completion re-ranking"
    );
}

#[test]
fn completions_with_ai_fails_closed_on_invalid_excluded_path_globs() {
    let text_with_caret = r#"
class Secret {
  void bar() {}

  void m() {
    int x = 0;
    <|>
  }
}
"#;

    let caret_offset = text_with_caret
        .find(CARET)
        .expect("fixture must contain <|> caret marker");
    let text = text_with_caret.replace(CARET, "");
    let position = offset_to_position(&text, caret_offset);

    let mut db = InMemoryFileStore::new();
    let path = PathBuf::from("src/Main.java");
    let file = db.file_id_for_path(&path);
    db.set_file_text(file, text);

    let baseline = completions(&db, file, position);

    let mut config = AiConfig::default();
    config.enabled = true;
    config.features.completion_ranking = true;
    // Invalid glob pattern: should disable ranking (fail-closed).
    config.privacy.excluded_paths = vec!["[".to_string()];

    let llm = Arc::new(CountingLlmClient::default());
    let ranked = block_on(completions_with_ai(
        &db,
        file,
        position,
        &config,
        Some(llm.clone()),
    ));
    assert_eq!(
        llm.chat_calls(),
        0,
        "expected invalid glob patterns to disable LLM-backed ranking (fail-closed)"
    );
    assert_eq!(
        labels(&ranked),
        labels(&baseline),
        "expected invalid excluded_paths globs to disable AI completion re-ranking"
    );
}
