use std::sync::Arc;

use lsp_types::{CompletionParams, NumberOrString, TextDocumentIdentifier, TextDocumentPositionParams, Uri};
use nova_config::NovaConfig;
use nova_db::InMemoryFileStore;
use nova_ext::{CompletionItem, Diagnostic, ProjectId, Span};
use nova_ide::extensions::FrameworkAnalyzerAdapter;
use nova_lsp::NovaLspIdeState;
use nova_scheduler::CancellationToken;
use tempfile::TempDir;

const TEST_DIAG_CODE: &str = "TEST_FRAMEWORK_DIAGNOSTIC";

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
            TEST_DIAG_CODE,
            "framework analyzer ran",
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
fn framework_analyzer_runs_via_extension_registry_in_lsp_pipeline_and_respects_cancellation() {
    let temp = TempDir::new().expect("tempdir");
    let path = temp.path().join("Test.java");

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
        FrameworkAnalyzerAdapter::new("test.framework", TestFrameworkAnalyzer).into_arc();
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

    // --- Diagnostics: analyzer runs through the normal LSP/IDE aggregation.
    let diags = state.diagnostics(CancellationToken::new(), &uri);
    assert!(
        diags.iter().any(|diag| matches!(
            &diag.code,
            Some(NumberOrString::String(code)) if code == TEST_DIAG_CODE
        )),
        "expected framework analyzer diagnostic; got {diags:?}"
    );

    // --- Completions: analyzer completions are aggregated too.
    let completion_params = CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            position: lsp_types::Position::new(3, 6),
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
        context: None,
    };

    let resp = state
        .completion(CancellationToken::new(), completion_params)
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

    // --- Cancellation: cancelled token should prevent analyzer results from being aggregated.
    let cancelled = CancellationToken::new();
    cancelled.cancel();

    let cancelled_diags = state.diagnostics(cancelled.clone(), &uri);
    assert!(
        cancelled_diags.is_empty()
            || !cancelled_diags.iter().any(|diag| matches!(
                &diag.code,
                Some(NumberOrString::String(code)) if code == TEST_DIAG_CODE
            )),
        "expected cancelled diagnostics to omit framework analyzer results; got {cancelled_diags:?}"
    );

    let cancelled_completion_params = CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri },
            position: lsp_types::Position::new(3, 6),
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
        context: None,
    };

    let cancelled_resp = state
        .completion(cancelled, cancelled_completion_params)
        .expect("completion response");
    let cancelled_items = match cancelled_resp {
        lsp_types::CompletionResponse::Array(items) => items,
        lsp_types::CompletionResponse::List(list) => list.items,
    };
    let cancelled_labels: Vec<_> = cancelled_items
        .iter()
        .map(|item| item.label.as_str())
        .collect();
    assert!(
        !cancelled_labels.contains(&"frameworkCompletion"),
        "expected cancelled completion response to omit framework completion item; got {cancelled_labels:?}"
    );
}

