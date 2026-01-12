use std::sync::Arc;

use lsp_types::{CompletionParams, TextDocumentIdentifier, TextDocumentPositionParams, Uri};
use nova_config::NovaConfig;
use nova_db::InMemoryFileStore;
use nova_ext::{CompletionItem, Diagnostic, ProjectId, Span};
use nova_ide::extensions::FrameworkAnalyzerAdapter;
use nova_lsp::NovaLspIdeState;
use nova_scheduler::CancellationToken;
use tempfile::TempDir;

struct TestFrameworkAnalyzer;

impl nova_framework::FrameworkAnalyzer for TestFrameworkAnalyzer {
    fn applies_to(&self, _db: &dyn nova_framework::Database, _project: ProjectId) -> bool {
        true
    }

    fn diagnostics(
        &self,
        db: &dyn nova_framework::Database,
        file: nova_ext::FileId,
    ) -> Vec<Diagnostic> {
        let _ = db.file_text(file);
        vec![Diagnostic::warning(
            "FW",
            "framework diagnostic",
            Some(Span::new(0, 1)),
        )]
    }

    fn completions(
        &self,
        db: &dyn nova_framework::Database,
        ctx: &nova_framework::CompletionContext,
    ) -> Vec<CompletionItem> {
        let _ = db.file_text(ctx.file);
        vec![CompletionItem::new("frameworkCompletion")]
    }
}

#[test]
fn lsp_extensions_can_register_framework_analyzers_via_adapter() {
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

    let analyzer =
        FrameworkAnalyzerAdapter::new("framework.test", TestFrameworkAnalyzer).into_arc();
    state
        .registry_mut()
        .register_diagnostic_provider(analyzer.clone())
        .unwrap();
    state
        .registry_mut()
        .register_completion_provider(analyzer.clone())
        .unwrap();

    let uri: Uri = url::Url::from_file_path(&path)
        .expect("file path to url")
        .as_str()
        .parse()
        .expect("parse uri");

    let diags = state.diagnostics(CancellationToken::new(), &uri);
    assert!(
        diags.iter().any(|d| d.message == "framework diagnostic"),
        "expected framework diagnostic; got {diags:?}"
    );

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
        labels.contains(&"frameworkCompletion"),
        "expected framework completion item; got {labels:?}"
    );
}
