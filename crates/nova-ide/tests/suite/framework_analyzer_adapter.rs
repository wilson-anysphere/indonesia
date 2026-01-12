use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use nova_config::NovaConfig;
use nova_db::InMemoryFileStore;
use nova_ext::{CompletionItem, Diagnostic, ProjectId, Span, Symbol};
use nova_framework::{CompletionContext, Database, FrameworkAnalyzer};
use nova_ide::extensions::{FrameworkAnalyzerAdapterOnTextDb, IdeExtensions};
use nova_scheduler::CancellationToken;

struct TestAnalyzer;

impl FrameworkAnalyzer for TestAnalyzer {
    fn applies_to(&self, _db: &dyn Database, _project: ProjectId) -> bool {
        true
    }

    fn diagnostics(&self, _db: &dyn Database, _file: nova_ext::FileId) -> Vec<Diagnostic> {
        vec![Diagnostic::warning(
            "TEST",
            "test diagnostic",
            Some(Span::new(0, 1)),
        )]
    }

    fn completions(&self, _db: &dyn Database, _ctx: &CompletionContext) -> Vec<CompletionItem> {
        vec![CompletionItem::new("test completion")]
    }

    fn navigation(
        &self,
        _db: &dyn Database,
        symbol: &nova_framework::Symbol,
    ) -> Vec<nova_framework::NavigationTarget> {
        let file = match *symbol {
            nova_framework::Symbol::File(file) => file,
            nova_framework::Symbol::Class(_) => nova_ext::FileId::from_raw(0),
        };
        vec![nova_framework::NavigationTarget {
            file,
            span: Some(Span::new(0, 1)),
            label: "test navigation".to_string(),
        }]
    }

    fn inlay_hints(
        &self,
        _db: &dyn Database,
        _file: nova_ext::FileId,
    ) -> Vec<nova_framework::InlayHint> {
        vec![nova_framework::InlayHint {
            span: Some(Span::new(0, 1)),
            label: "test hint".to_string(),
        }]
    }
}

#[test]
fn framework_analyzer_adapter_on_text_db_surfaces_results() {
    // Pseudo workspace root with a Java file.
    let mut db = InMemoryFileStore::new();
    let file = db.file_id_for_path(PathBuf::from("/workspace/src/main/java/com/example/A.java"));
    db.set_file_text(file, "class A {}".to_string());

    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(db);
    let mut ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), ProjectId::new(0));

    let adapter = FrameworkAnalyzerAdapterOnTextDb::new("framework.test", TestAnalyzer).into_arc();
    ide.registry_mut()
        .register_diagnostic_provider(adapter.clone())
        .unwrap();
    ide.registry_mut()
        .register_completion_provider(adapter.clone())
        .unwrap();
    ide.registry_mut()
        .register_navigation_provider(adapter.clone())
        .unwrap();
    ide.registry_mut()
        .register_inlay_hint_provider(adapter.clone())
        .unwrap();

    let diags = ide.diagnostics(CancellationToken::new(), file);
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0].message, "test diagnostic");

    let completions = ide.completions(CancellationToken::new(), file, 0);
    assert_eq!(completions.len(), 1);
    assert_eq!(completions[0].label, "test completion");

    let nav = ide.navigation(CancellationToken::new(), Symbol::File(file));
    assert_eq!(nav.len(), 1);
    assert_eq!(nav[0].label, "test navigation");
    assert_eq!(nav[0].file, file);

    let hints = ide.inlay_hints(CancellationToken::new(), file);
    assert_eq!(hints.len(), 1);
    assert_eq!(hints[0].label, "test hint");
}

#[test]
fn framework_analyzer_adapter_on_text_db_propagates_cancellation_to_analyzer() {
    struct CancellationAwareAnalyzer {
        diagnostics_started: mpsc::Sender<()>,
        diagnostics_finished: mpsc::Sender<()>,
        completions_started: mpsc::Sender<()>,
        completions_finished: mpsc::Sender<()>,
        saw_diagnostics_cancel: Arc<AtomicBool>,
        saw_completions_cancel: Arc<AtomicBool>,
    }

    impl FrameworkAnalyzer for CancellationAwareAnalyzer {
        fn applies_to(&self, _db: &dyn Database, _project: ProjectId) -> bool {
            true
        }

        fn diagnostics(&self, _db: &dyn Database, _file: nova_ext::FileId) -> Vec<Diagnostic> {
            vec![Diagnostic::warning(
                "TEST",
                "diagnostics should not run without cancellation support",
                Some(Span::new(0, 1)),
            )]
        }

        fn diagnostics_with_cancel(
            &self,
            _db: &dyn Database,
            _file: nova_ext::FileId,
            cancel: &CancellationToken,
        ) -> Vec<Diagnostic> {
            let _ = self.diagnostics_started.send(());
            for _ in 0..250 {
                if cancel.is_cancelled() {
                    self.saw_diagnostics_cancel.store(true, Ordering::SeqCst);
                    let _ = self.diagnostics_finished.send(());
                    return Vec::new();
                }
                std::thread::sleep(Duration::from_millis(1));
            }

            let _ = self.diagnostics_finished.send(());
            vec![Diagnostic::warning(
                "TEST",
                "should-have-been-cancelled",
                Some(Span::new(0, 1)),
            )]
        }

        fn completions(&self, _db: &dyn Database, _ctx: &CompletionContext) -> Vec<CompletionItem> {
            vec![CompletionItem::new(
                "completions should not run without cancellation support",
            )]
        }

        fn completions_with_cancel(
            &self,
            _db: &dyn Database,
            _ctx: &CompletionContext,
            cancel: &CancellationToken,
        ) -> Vec<CompletionItem> {
            let _ = self.completions_started.send(());
            for _ in 0..250 {
                if cancel.is_cancelled() {
                    self.saw_completions_cancel.store(true, Ordering::SeqCst);
                    let _ = self.completions_finished.send(());
                    return Vec::new();
                }
                std::thread::sleep(Duration::from_millis(1));
            }

            let _ = self.completions_finished.send(());
            vec![CompletionItem::new("should-have-been-cancelled")]
        }
    }

    // Pseudo workspace root with a Java file.
    let mut db = InMemoryFileStore::new();
    let file = db.file_id_for_path(PathBuf::from("/workspace/src/main/java/com/example/A.java"));
    db.set_file_text(file, "class A {}".to_string());

    let (diag_started_tx, diag_started_rx) = mpsc::channel();
    let (diag_finished_tx, diag_finished_rx) = mpsc::channel();
    let (completion_started_tx, completion_started_rx) = mpsc::channel();
    let (completion_finished_tx, completion_finished_rx) = mpsc::channel();
    let saw_diag_cancel = Arc::new(AtomicBool::new(false));
    let saw_completion_cancel = Arc::new(AtomicBool::new(false));

    let analyzer = FrameworkAnalyzerAdapterOnTextDb::new(
        "framework.cancel",
        CancellationAwareAnalyzer {
            diagnostics_started: diag_started_tx,
            diagnostics_finished: diag_finished_tx,
            completions_started: completion_started_tx,
            completions_finished: completion_finished_tx,
            saw_diagnostics_cancel: Arc::clone(&saw_diag_cancel),
            saw_completions_cancel: Arc::clone(&saw_completion_cancel),
        },
    )
    .into_arc();

    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(db);
    let mut ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), ProjectId::new(0));
    ide.registry_mut().options_mut().diagnostic_timeout = Duration::from_secs(1);
    ide.registry_mut().options_mut().completion_timeout = Duration::from_secs(1);
    ide.registry_mut()
        .register_diagnostic_provider(analyzer.clone())
        .unwrap();
    ide.registry_mut()
        .register_completion_provider(analyzer)
        .unwrap();

    let cancel = CancellationToken::new();
    let cancel_for_thread = cancel.clone();
    let cancel_thread = std::thread::spawn(move || {
        diag_started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("diagnostics should start");
        cancel_for_thread.cancel();
    });

    let diags = ide.diagnostics(cancel, file);
    assert!(diags.is_empty());

    cancel_thread.join().unwrap();
    diag_finished_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("diagnostics should finish after cancellation");
    assert!(saw_diag_cancel.load(Ordering::SeqCst));

    let cancel = CancellationToken::new();
    let cancel_for_thread = cancel.clone();
    let cancel_thread = std::thread::spawn(move || {
        completion_started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("completions should start");
        cancel_for_thread.cancel();
    });

    let completions = ide.completions(cancel, file, 0);
    assert!(completions.is_empty());

    cancel_thread.join().unwrap();
    completion_finished_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("completions should finish after cancellation");
    assert!(saw_completion_cancel.load(Ordering::SeqCst));
}
