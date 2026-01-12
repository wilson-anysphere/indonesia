use std::path::PathBuf;
use std::sync::Arc;

use nova_config::NovaConfig;
use nova_db::InMemoryFileStore;
use nova_ext::{ExtensionRegistry, ProjectId, Span, Symbol};
use nova_framework::{Database as FrameworkDatabase, FrameworkAnalyzer};
use nova_scheduler::CancellationToken;

use nova_ide::extensions::{FrameworkAnalyzerAdapter, FrameworkIdeDatabase, IdeExtensions};

struct TestFrameworkAnalyzer;

impl FrameworkAnalyzer for TestFrameworkAnalyzer {
    fn applies_to(&self, _db: &dyn FrameworkDatabase, _project: ProjectId) -> bool {
        true
    }

    fn diagnostics(
        &self,
        db: &dyn FrameworkDatabase,
        file: nova_ext::FileId,
    ) -> Vec<nova_ext::Diagnostic> {
        let text = db.file_text(file).unwrap_or("");
        vec![nova_ext::Diagnostic::warning(
            "FW_IDE_DB",
            text.to_string(),
            Some(Span::new(0, 1)),
        )]
    }

    fn completions(
        &self,
        _db: &dyn FrameworkDatabase,
        _ctx: &nova_framework::CompletionContext,
    ) -> Vec<nova_ext::CompletionItem> {
        vec![nova_ext::CompletionItem::new("fwIdeDbCompletion")]
    }

    fn navigation(
        &self,
        _db: &dyn FrameworkDatabase,
        symbol: &nova_framework::Symbol,
    ) -> Vec<nova_framework::NavigationTarget> {
        match *symbol {
            nova_framework::Symbol::File(file) => vec![nova_framework::NavigationTarget {
                file,
                span: Some(Span::new(0, 1)),
                label: "fwIdeDbNav".to_string(),
            }],
            nova_framework::Symbol::Class(_) => Vec::new(),
        }
    }

    fn inlay_hints(
        &self,
        _db: &dyn FrameworkDatabase,
        _file: nova_ext::FileId,
    ) -> Vec<nova_framework::InlayHint> {
        vec![nova_framework::InlayHint {
            span: Some(Span::new(0, 1)),
            label: "fwIdeDbHint".to_string(),
        }]
    }
}

#[test]
fn framework_analyzer_adapter_runs_on_framework_ide_database() {
    let mut db = InMemoryFileStore::new();
    let file_path = PathBuf::from("/workspace/src/main/java/Main.java");
    let file = db.file_id_for_path(&file_path);
    db.set_file_text(file, "class Main {}".to_string());

    let inner: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(db);
    let fw_db = FrameworkIdeDatabase::new(inner, ProjectId::new(0)).into_arc();

    let analyzer =
        FrameworkAnalyzerAdapter::new("framework.test_fw_ide_db", TestFrameworkAnalyzer).into_arc();

    let mut registry: ExtensionRegistry<FrameworkIdeDatabase> = ExtensionRegistry::default();
    registry
        .register_diagnostic_provider(analyzer.clone())
        .expect("register diagnostic provider");
    registry
        .register_completion_provider(analyzer.clone())
        .expect("register completion provider");
    registry
        .register_navigation_provider(analyzer.clone())
        .expect("register navigation provider");
    registry
        .register_inlay_hint_provider(analyzer)
        .expect("register inlay hint provider");

    let ide = IdeExtensions::with_registry(
        fw_db,
        Arc::new(NovaConfig::default()),
        ProjectId::new(0),
        registry,
    );

    let diags = ide.diagnostics(CancellationToken::new(), file);
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0].code.as_ref(), "FW_IDE_DB");
    assert_eq!(diags[0].message, "class Main {}");

    let completions = ide.completions(CancellationToken::new(), file, 0);
    assert_eq!(completions.len(), 1);
    assert_eq!(completions[0].label, "fwIdeDbCompletion");

    let nav = ide.navigation(CancellationToken::new(), Symbol::File(file));
    assert_eq!(nav.len(), 1);
    assert_eq!(nav[0].label, "fwIdeDbNav");

    let hints = ide.inlay_hints(CancellationToken::new(), file);
    assert_eq!(hints.len(), 1);
    assert_eq!(hints[0].label, "fwIdeDbHint");
}

