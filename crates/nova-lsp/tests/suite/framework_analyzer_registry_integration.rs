use std::sync::Arc;

use lsp_types::{CompletionParams, TextDocumentIdentifier, TextDocumentPositionParams};
use nova_config::NovaConfig;
use nova_db::InMemoryFileStore;
use nova_ext::ProjectId;
use nova_framework::{CompletionContext, Database as FrameworkDatabase, FrameworkAnalyzer};
use nova_ide::extensions::FrameworkAnalyzerOnTextDbAdapter;
use nova_lsp::NovaLspIdeState;
use nova_scheduler::CancellationToken;
use nova_types::{CompletionItem as NovaCompletionItem, Diagnostic as NovaDiagnostic, Span};
use tempfile::TempDir;

struct TestFrameworkAnalyzer;

impl FrameworkAnalyzer for TestFrameworkAnalyzer {
    fn applies_to(&self, _db: &dyn FrameworkDatabase, _project: nova_core::ProjectId) -> bool {
        true
    }

    fn diagnostics_with_cancel(
        &self,
        _db: &dyn FrameworkDatabase,
        _file: nova_vfs::FileId,
        cancel: &CancellationToken,
    ) -> Vec<NovaDiagnostic> {
        if cancel.is_cancelled() {
            return Vec::new();
        }

        vec![NovaDiagnostic::warning(
            "FW_LSP",
            "framework analyzer diagnostic via ExtensionRegistry",
            Some(Span::new(0, 1)),
        )]
    }

    fn completions_with_cancel(
        &self,
        _db: &dyn FrameworkDatabase,
        _ctx: &CompletionContext,
        cancel: &CancellationToken,
    ) -> Vec<NovaCompletionItem> {
        if cancel.is_cancelled() {
            return Vec::new();
        }

        vec![NovaCompletionItem::new("fwLspCompletion")]
    }
}

fn has_diagnostic_code(diags: &[lsp_types::Diagnostic], code: &str) -> bool {
    diags.iter().any(|diag| match diag.code.as_ref() {
        Some(lsp_types::NumberOrString::String(value)) => value == code,
        _ => false,
    })
}

#[test]
fn framework_analyzer_registry_integration_runs_and_respects_cancellation() {
    let temp = TempDir::new().expect("tempdir");
    let path = temp.path().join("FrameworkAnalyzer.java");

    let source = "class A {}";

    let mut db = InMemoryFileStore::new();
    let file = db.file_id_for_path(&path);
    db.set_file_text(file, source.to_string());

    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(db);
    let config = Arc::new(NovaConfig::default());

    let mut state = NovaLspIdeState::new(db, config, ProjectId::new(0));

    let analyzer = TestFrameworkAnalyzer;
    let provider = FrameworkAnalyzerOnTextDbAdapter::new("test.framework.lsp", analyzer).into_arc();
    state
        .registry_mut()
        .register_diagnostic_provider(provider.clone())
        .unwrap();
    state
        .registry_mut()
        .register_completion_provider(provider)
        .unwrap();

    let uri: lsp_types::Uri = url::Url::from_file_path(&path)
        .expect("file path to url")
        .as_str()
        .parse()
        .expect("parse uri");

    // Diagnostics path: ensure the framework analyzer runs through `NovaLspIdeState` and the
    // `ExtensionRegistry` aggregation.
    let diagnostics = state.diagnostics(CancellationToken::new(), &uri);
    assert!(
        has_diagnostic_code(&diagnostics, "FW_LSP"),
        "expected framework diagnostic; got {diagnostics:?}"
    );

    // Completion path: ensure completion results are surfaced via LSP.
    let params = CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            position: lsp_types::Position::new(0, 0),
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
        labels.contains(&"fwLspCompletion"),
        "expected framework completion item; got {labels:?}"
    );

    // Cancellation: cancelled requests should not surface framework results.
    let cancelled = CancellationToken::new();
    cancelled.cancel();

    let diagnostics_cancelled = state.diagnostics(cancelled.clone(), &uri);
    assert!(
        !has_diagnostic_code(&diagnostics_cancelled, "FW_LSP"),
        "expected framework diagnostic to be suppressed when cancelled; got {diagnostics_cancelled:?}"
    );

    let params_cancelled = CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri },
            position: lsp_types::Position::new(0, 0),
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
        context: None,
    };

    let resp_cancelled = state
        .completion(cancelled, params_cancelled)
        .expect("completion response");

    let items_cancelled = match resp_cancelled {
        lsp_types::CompletionResponse::Array(items) => items,
        lsp_types::CompletionResponse::List(list) => list.items,
    };

    let labels_cancelled: Vec<_> = items_cancelled
        .iter()
        .map(|item| item.label.as_str())
        .collect();
    assert!(
        !labels_cancelled.contains(&"fwLspCompletion"),
        "expected framework completion item to be suppressed when cancelled; got {labels_cancelled:?}"
    );
}
