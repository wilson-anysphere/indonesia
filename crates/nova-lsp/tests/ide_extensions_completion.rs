use std::sync::Arc;

use lsp_types::{CompletionParams, TextDocumentIdentifier, TextDocumentPositionParams};
use nova_config::NovaConfig;
use nova_db::InMemoryFileStore;
use nova_ext::{
    CompletionItem, CompletionParams as ExtCompletionParams, CompletionProvider, ExtensionContext,
    ProjectId,
};
use nova_lsp::NovaLspIdeState;
use nova_scheduler::CancellationToken;
use tempfile::TempDir;

struct ExtraCompletionProvider;

impl CompletionProvider<nova_lsp::DynDb> for ExtraCompletionProvider {
    fn id(&self) -> &str {
        "test.extra-completion"
    }

    fn provide_completions(
        &self,
        _ctx: ExtensionContext<nova_lsp::DynDb>,
        _params: ExtCompletionParams,
    ) -> Vec<CompletionItem> {
        vec![CompletionItem::new("extraCompletion")]
    }
}

#[test]
fn completion_merges_builtin_and_extension_items() {
    let temp = TempDir::new().expect("tempdir");
    let path = temp.path().join("Completion.java");

    let source = r#"class A {
  void m() {
    String s = "";
    s.
  }
}
"#;

    let mut db = InMemoryFileStore::new();
    let file = db.file_id_for_path(&path);
    db.set_file_text(file, source.to_string());

    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(db);
    let config = Arc::new(NovaConfig::default());

    let mut state = NovaLspIdeState::new(db, config, ProjectId::new(0));
    state
        .registry_mut()
        .register_completion_provider(Arc::new(ExtraCompletionProvider))
        .unwrap();

    let uri: lsp_types::Uri = url::Url::from_file_path(&path)
        .expect("file path to url")
        .as_str()
        .parse()
        .expect("parse uri");
    let params = CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri },
            position: lsp_types::Position::new(3, 6),
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
        context: None,
    };

    let resp = state
        .completion(CancellationToken::new(), params)
        .expect("completion response");

    let items = match resp {
        lsp_types::CompletionResponse::Array(items) => items,
        lsp_types::CompletionResponse::List(list) => list.items,
    };

    let labels: Vec<_> = items.iter().map(|item| item.label.as_str()).collect();
    assert!(
        labels.contains(&"length"),
        "expected built-in completion item; got {labels:?}"
    );
    assert!(
        labels.contains(&"extraCompletion"),
        "expected extension completion item; got {labels:?}"
    );
}
