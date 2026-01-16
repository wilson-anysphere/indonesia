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

#[test]
fn per_analyzer_providers_isolate_timeouts_and_panics() {
    struct SlowOrPanickingAnalyzer;

    impl FrameworkAnalyzer for SlowOrPanickingAnalyzer {
        fn applies_to(&self, _db: &dyn FrameworkDatabase, _project: ProjectId) -> bool {
            true
        }

        fn diagnostics_with_cancel(
            &self,
            _db: &dyn FrameworkDatabase,
            _file: nova_ext::FileId,
            cancel: &CancellationToken,
        ) -> Vec<nova_ext::Diagnostic> {
            // Wait until the extension watchdog cancels us (triggering a timeout).
            while !cancel.is_cancelled() {
                std::thread::sleep(Duration::from_millis(1));
            }

            // If we cooperatively observe cancellation, return nothing. The provider invocation has
            // already been timed out by the registry watchdog.
            Vec::new()
        }

        fn completions_with_cancel(
            &self,
            _db: &dyn FrameworkDatabase,
            _ctx: &nova_framework::CompletionContext,
            _cancel: &CancellationToken,
        ) -> Vec<nova_ext::CompletionItem> {
            panic!("boom");
        }
    }

    struct FastAnalyzer;

    impl FrameworkAnalyzer for FastAnalyzer {
        fn applies_to(&self, _db: &dyn FrameworkDatabase, _project: ProjectId) -> bool {
            true
        }

        fn diagnostics(
            &self,
            _db: &dyn FrameworkDatabase,
            _file: nova_ext::FileId,
        ) -> Vec<nova_ext::Diagnostic> {
            vec![nova_ext::Diagnostic::warning(
                "FAST",
                "fast diagnostic",
                Some(Span::new(0, 1)),
            )]
        }

        fn completions(
            &self,
            _db: &dyn FrameworkDatabase,
            _ctx: &nova_framework::CompletionContext,
        ) -> Vec<nova_ext::CompletionItem> {
            vec![nova_ext::CompletionItem::new("fastCompletion")]
        }
    }

    let mut db = InMemoryFileStore::new();
    let file = db.file_id_for_path(PathBuf::from("/workspace/src/main/java/A.java"));
    db.set_file_text(file, "class A {}".to_string());
    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(db);

    // Warm the `framework_db` cache so the fast analyzer doesn't occasionally hit the watchdog
    // timeout due to first-use indexing overhead under parallel test execution.
    let _ = nova_ide::framework_db::framework_db_for_file(
        Arc::clone(&db),
        file,
        &CancellationToken::new(),
    );

    let slow = FrameworkAnalyzerAdapterOnTextDb::new("a.slow", SlowOrPanickingAnalyzer).into_arc();
    let fast = FrameworkAnalyzerAdapterOnTextDb::new("b.fast", FastAnalyzer).into_arc();

    let mut registry: ExtensionRegistry<dyn nova_db::Database + Send + Sync> =
        ExtensionRegistry::default();
    registry
        .register_diagnostic_provider(slow.clone())
        .expect("register slow diagnostic provider");
    registry
        .register_completion_provider(slow.clone())
        .expect("register slow completion provider");
    registry
        .register_diagnostic_provider(fast.clone())
        .expect("register fast diagnostic provider");
    registry
        .register_completion_provider(fast.clone())
        .expect("register fast completion provider");

    let ide = IdeExtensions::with_registry(
        db,
        Arc::new(NovaConfig::default()),
        ProjectId::new(0),
        registry,
    );

    // Trigger enough timeouts to open the circuit breaker for the slow analyzer.
    for _ in 0..4 {
        let diags = ide.diagnostics(CancellationToken::new(), file);
        assert_eq!(
            diags.len(),
            1,
            "expected only the fast analyzer diagnostic to surface; got {diags:#?}"
        );
        assert_eq!(diags[0].code.as_ref(), "FAST");
    }

    let completions = ide.completions(CancellationToken::new(), file, 0);
    assert_eq!(
        completions.len(),
        1,
        "expected only the fast analyzer completions to surface; got {completions:#?}"
    );
    assert_eq!(completions[0].label, "fastCompletion");

    let stats = ide.registry().stats();
    let slow_diag = stats
        .diagnostic
        .get("a.slow")
        .expect("stats for slow diagnostics provider");
    assert_eq!(slow_diag.calls_total, 3, "expected 3 timed-out calls");
    assert_eq!(
        slow_diag.timeouts_total, 3,
        "expected all calls to time out"
    );
    assert_eq!(
        slow_diag.skipped_total, 1,
        "expected provider to be skipped after circuit opens"
    );

    let fast_diag = stats
        .diagnostic
        .get("b.fast")
        .expect("stats for fast diagnostics provider");
    assert_eq!(fast_diag.calls_total, 4);
    assert_eq!(fast_diag.timeouts_total, 0);
    assert_eq!(fast_diag.panics_total, 0);

    let slow_completion = stats
        .completion
        .get("a.slow")
        .expect("stats for slow completion provider");
    assert_eq!(slow_completion.calls_total, 1);
    assert_eq!(slow_completion.panics_total, 1);

    let fast_completion = stats
        .completion
        .get("b.fast")
        .expect("stats for fast completion provider");
    assert_eq!(fast_completion.calls_total, 1);
    assert_eq!(fast_completion.panics_total, 0);
}
