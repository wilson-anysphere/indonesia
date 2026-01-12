use nova_config::NovaConfig;
use nova_db::InMemoryFileStore;
use nova_ide::extensions::IdeExtensions;
use nova_scheduler::CancellationToken;
use pretty_assertions::assert_eq;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use nova_ext::{
    CompletionItem, CompletionParams, CompletionProvider, Diagnostic, DiagnosticParams,
    DiagnosticProvider, ExtensionContext, ProjectId, Span,
};
use nova_ide::extensions::FrameworkIdeDatabase;

struct TestCompletionProvider {
    id: &'static str,
    label: &'static str,
}

impl CompletionProvider<nova_lsp::DynDb> for TestCompletionProvider {
    fn id(&self) -> &str {
        self.id
    }

    fn provide_completions(
        &self,
        _ctx: ExtensionContext<nova_lsp::DynDb>,
        _params: CompletionParams,
    ) -> Vec<CompletionItem> {
        vec![CompletionItem::new(self.label)]
    }
}

struct TestDiagnosticProvider {
    id: &'static str,
    message: &'static str,
}

impl DiagnosticProvider<nova_lsp::DynDb> for TestDiagnosticProvider {
    fn id(&self) -> &str {
        self.id
    }

    fn provide_diagnostics(
        &self,
        _ctx: ExtensionContext<nova_lsp::DynDb>,
        _params: DiagnosticParams,
    ) -> Vec<Diagnostic> {
        vec![Diagnostic::warning(
            "EXT",
            self.message,
            Some(Span::new(0, 1)),
        )]
    }
}

#[test]
fn completion_with_extensions_merges_results_in_deterministic_order() {
    let mut db = InMemoryFileStore::new();
    let file = db.file_id_for_path(PathBuf::from("/completion.java"));
    db.set_file_text(
        file,
        r#"
class A {
  void m() {
    String s = "";
    s.
  }
}
"#
        .to_string(),
    );

    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(db);
    let lsp_db: Arc<nova_lsp::DynDb> = Arc::new(FrameworkIdeDatabase::new(
        Arc::clone(&db),
        ProjectId::new(0),
    ));
    let mut extensions =
        IdeExtensions::new(lsp_db, Arc::new(NovaConfig::default()), ProjectId::new(0));
    extensions
        .registry_mut()
        .register_completion_provider(Arc::new(TestCompletionProvider {
            id: "b.completion",
            label: "from-b",
        }))
        .unwrap();
    extensions
        .registry_mut()
        .register_completion_provider(Arc::new(TestCompletionProvider {
            id: "a.completion",
            label: "from-a",
        }))
        .unwrap();

    // Avoid flakiness under heavy test parallelism: providers are executed with a per-call
    // watchdog timeout and the default is tuned for interactive latency, not CI environments.
    extensions.registry_mut().options_mut().completion_timeout = Duration::from_secs(1);

    let position = lsp_types::Position::new(4, 6);
    let built_in = nova_lsp::completion(db.as_ref(), file, position);
    assert!(
        built_in.iter().any(|c| c.label == "length"),
        "expected built-in String member completion; got {built_in:?}"
    );

    let out =
        nova_lsp::completion_with_extensions(&extensions, CancellationToken::new(), file, position);

    assert_eq!(&out[..built_in.len()], built_in.as_slice());
    assert_eq!(
        out[built_in.len()..]
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        vec!["from-a", "from-b"],
    );
}

#[test]
fn diagnostics_with_extensions_merges_results_in_deterministic_order() {
    let mut db = InMemoryFileStore::new();
    let file = db.file_id_for_path(PathBuf::from("/diagnostics.java"));
    db.set_file_text(
        file,
        r#"
class A {
  void m() {
    baz();
  }
}
"#
        .to_string(),
    );

    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(db);
    let lsp_db: Arc<nova_lsp::DynDb> = Arc::new(FrameworkIdeDatabase::new(
        Arc::clone(&db),
        ProjectId::new(0),
    ));
    let mut extensions =
        IdeExtensions::new(lsp_db, Arc::new(NovaConfig::default()), ProjectId::new(0));
    extensions
        .registry_mut()
        .register_diagnostic_provider(Arc::new(TestDiagnosticProvider {
            id: "b.diagnostic",
            message: "from-b",
        }))
        .unwrap();
    extensions
        .registry_mut()
        .register_diagnostic_provider(Arc::new(TestDiagnosticProvider {
            id: "a.diagnostic",
            message: "from-a",
        }))
        .unwrap();

    extensions.registry_mut().options_mut().diagnostic_timeout = Duration::from_secs(1);

    let built_in = nova_lsp::diagnostics(db.as_ref(), file);
    assert!(
        built_in
            .iter()
            .any(|d| d.message.contains("Cannot resolve symbol 'baz'")),
        "expected built-in unresolved reference diagnostic; got {built_in:?}"
    );

    let out = nova_lsp::diagnostics_with_extensions(&extensions, CancellationToken::new(), file);

    fn diag_key(diag: &lsp_types::Diagnostic) -> (u32, u32, u32, u32, String, String) {
        let code = diag
            .code
            .as_ref()
            .map(|code| match code {
                lsp_types::NumberOrString::Number(n) => n.to_string(),
                lsp_types::NumberOrString::String(s) => s.clone(),
            })
            .unwrap_or_default();
        (
            diag.range.start.line,
            diag.range.start.character,
            diag.range.end.line,
            diag.range.end.character,
            code,
            diag.message.clone(),
        )
    }

    let mut built_in_sorted = built_in.clone();
    built_in_sorted.sort_by_key(diag_key);
    let mut out_builtin_sorted = out[..built_in.len()].to_vec();
    out_builtin_sorted.sort_by_key(diag_key);
    assert_eq!(out_builtin_sorted, built_in_sorted);
    assert_eq!(
        out[built_in.len()..]
            .iter()
            .map(|diag| diag.message.as_str())
            .collect::<Vec<_>>(),
        vec!["from-a", "from-b"],
    );
}
