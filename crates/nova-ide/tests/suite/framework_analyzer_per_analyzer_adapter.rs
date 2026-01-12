use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use nova_config::NovaConfig;
use nova_db::InMemoryFileStore;
use nova_ext::{ClassId, ExtensionRegistry, ProjectId, Span, Symbol};
use nova_framework::{Database as FrameworkDatabase, FrameworkAnalyzer};
use nova_scheduler::CancellationToken;

use nova_ide::extensions::{FrameworkAnalyzerAdapterOnTextDb, IdeExtensions};

struct TestFrameworkAnalyzer;

impl FrameworkAnalyzer for TestFrameworkAnalyzer {
    fn applies_to(&self, _db: &dyn FrameworkDatabase, _project: ProjectId) -> bool {
        true
    }

    fn diagnostics(
        &self,
        _db: &dyn FrameworkDatabase,
        _file: nova_ext::FileId,
    ) -> Vec<nova_ext::Diagnostic> {
        vec![nova_ext::Diagnostic::warning(
            "FW_ADAPTER",
            "framework adapter diagnostic",
            Some(Span::new(0, 1)),
        )]
    }

    fn completions(
        &self,
        _db: &dyn FrameworkDatabase,
        _ctx: &nova_framework::CompletionContext,
    ) -> Vec<nova_ext::CompletionItem> {
        vec![nova_ext::CompletionItem::new("fwAdapterCompletion")]
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
                label: "fwNavTarget".to_string(),
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
            label: "fwHint".to_string(),
        }]
    }
}

#[test]
fn framework_analyzer_adapter_runs_on_host_db_as_per_analyzer_providers() {
    let mut db = InMemoryFileStore::new();
    let file = db.file_id_for_path(PathBuf::from("/workspace/src/main/java/A.java"));
    db.set_file_text(file, "class A {}".to_string());
    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(db);

    let analyzer =
        FrameworkAnalyzerAdapterOnTextDb::new("framework.test_adapter", TestFrameworkAnalyzer)
            .into_arc();

    let mut registry: ExtensionRegistry<dyn nova_db::Database + Send + Sync> =
        ExtensionRegistry::default();
    registry
        .register_diagnostic_provider(analyzer.clone())
        .expect("register diagnostic provider");
    registry
        .register_completion_provider(analyzer.clone())
        .expect("register completion provider");
    registry
        .register_inlay_hint_provider(analyzer.clone())
        .expect("register inlay hint provider");
    registry
        .register_navigation_provider(analyzer)
        .expect("register navigation provider");

    let ide = IdeExtensions::with_registry(
        db,
        Arc::new(NovaConfig::default()),
        ProjectId::new(0),
        registry,
    );

    let diags = ide.diagnostics(CancellationToken::new(), file);
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0].code.as_ref(), "FW_ADAPTER");

    let completions = ide.completions(CancellationToken::new(), file, 0);
    assert_eq!(completions.len(), 1);
    assert_eq!(completions[0].label, "fwAdapterCompletion");

    let hints = ide.inlay_hints(CancellationToken::new(), file);
    assert_eq!(hints.len(), 1);
    assert_eq!(hints[0].label, "fwHint");

    let nav = ide.navigation(CancellationToken::new(), Symbol::File(file));
    assert_eq!(nav.len(), 1);
    assert_eq!(nav[0].file, file);
    assert_eq!(nav[0].label, "fwNavTarget");
    assert_eq!(nav[0].span, Some(Span::new(0, 1)));
}

#[test]
fn framework_analyzer_adapter_propagates_cancellation_to_analyzer_on_host_db() {
    struct CancellationAwareAnalyzer {
        started: mpsc::Sender<()>,
        finished: mpsc::Sender<()>,
        saw_cancel: Arc<AtomicBool>,
    }

    impl FrameworkAnalyzer for CancellationAwareAnalyzer {
        fn applies_to(&self, _db: &dyn FrameworkDatabase, _project: ProjectId) -> bool {
            true
        }

        fn diagnostics_with_cancel(
            &self,
            _db: &dyn FrameworkDatabase,
            _file: nova_ext::FileId,
            cancel: &CancellationToken,
        ) -> Vec<nova_ext::Diagnostic> {
            let _ = self.started.send(());

            // Simulate some work that periodically checks for cancellation.
            for _ in 0..250 {
                if cancel.is_cancelled() {
                    self.saw_cancel.store(true, Ordering::SeqCst);
                    let _ = self.finished.send(());
                    return Vec::new();
                }
                std::thread::sleep(Duration::from_millis(1));
            }

            // If we never see cancellation, surface a diagnostic so the test fails.
            let _ = self.finished.send(());
            vec![nova_ext::Diagnostic::warning(
                "FW_ADAPTER",
                "should-have-been-cancelled",
                Some(Span::new(0, 1)),
            )]
        }
    }

    let mut db = InMemoryFileStore::new();
    let file = db.file_id_for_path(PathBuf::from("/workspace/src/main/java/A.java"));
    db.set_file_text(file, "class A {}".to_string());
    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(db);

    let (started_tx, started_rx) = mpsc::channel();
    let (finished_tx, finished_rx) = mpsc::channel();
    let saw_cancel = Arc::new(AtomicBool::new(false));

    let analyzer = FrameworkAnalyzerAdapterOnTextDb::new(
        "framework.cancel_adapter",
        CancellationAwareAnalyzer {
            started: started_tx,
            finished: finished_tx,
            saw_cancel: Arc::clone(&saw_cancel),
        },
    )
    .into_arc();

    let mut registry: ExtensionRegistry<dyn nova_db::Database + Send + Sync> =
        ExtensionRegistry::default();
    registry.options_mut().diagnostic_timeout = Duration::from_secs(1);
    registry
        .register_diagnostic_provider(analyzer)
        .expect("register diagnostic provider");

    let ide = IdeExtensions::with_registry(
        db,
        Arc::new(NovaConfig::default()),
        ProjectId::new(0),
        registry,
    );

    let cancel = CancellationToken::new();
    let cancel_for_thread = cancel.clone();

    let cancel_thread = std::thread::spawn(move || {
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("analyzer should start");
        cancel_for_thread.cancel();
    });

    let diags = ide.diagnostics(cancel, file);
    assert!(diags.is_empty());

    cancel_thread.join().unwrap();
    finished_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("analyzer should finish after cancellation");
    assert!(saw_cancel.load(Ordering::SeqCst));
}

#[test]
fn framework_analyzer_adapter_attempts_best_effort_class_navigation() {
    struct ClassNavAnalyzer {
        file: nova_ext::FileId,
    }

    impl FrameworkAnalyzer for ClassNavAnalyzer {
        fn applies_to(&self, _db: &dyn FrameworkDatabase, _project: ProjectId) -> bool {
            true
        }

        fn navigation(
            &self,
            _db: &dyn FrameworkDatabase,
            symbol: &nova_framework::Symbol,
        ) -> Vec<nova_framework::NavigationTarget> {
            match *symbol {
                nova_framework::Symbol::Class(_) => vec![nova_framework::NavigationTarget {
                    file: self.file,
                    span: Some(Span::new(2, 3)),
                    label: "fwClassNavTarget".to_string(),
                }],
                _ => Vec::new(),
            }
        }
    }

    let mut db = InMemoryFileStore::new();
    let file = db.file_id_for_path(PathBuf::from("/workspace/src/main/java/A.java"));
    db.set_file_text(file, "class A {}".to_string());
    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(db);

    let analyzer = FrameworkAnalyzerAdapterOnTextDb::new(
        "framework.class_nav_adapter",
        ClassNavAnalyzer { file },
    )
    .into_arc();

    let mut registry: ExtensionRegistry<dyn nova_db::Database + Send + Sync> =
        ExtensionRegistry::default();
    registry
        .register_navigation_provider(analyzer)
        .expect("register navigation provider");

    let ide = IdeExtensions::with_registry(
        db,
        Arc::new(NovaConfig::default()),
        ProjectId::new(0),
        registry,
    );

    // `framework_db` assigns dense class ids starting at 0 within a root; the fixture has one class.
    let targets = ide.navigation(
        CancellationToken::new(),
        Symbol::Class(ClassId::from_raw(0)),
    );
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].file, file);
    assert_eq!(targets[0].label, "fwClassNavTarget");
    assert_eq!(targets[0].span, Some(Span::new(2, 3)));
}
