use nova_config::NovaConfig;
use nova_ext::{
    CodeAction, CodeActionParams, CompletionItem, CompletionParams, Diagnostic, DiagnosticParams,
    ExtensionContext, ExtensionRegistry, InlayHint, InlayHintParams, NavigationParams,
    NavigationTarget, ProjectId, Span, Symbol,
};
use nova_scheduler::CancellationToken;
use std::sync::Arc;

pub struct IdeExtensions<DB: ?Sized + Send + Sync + 'static> {
    db: Arc<DB>,
    config: Arc<NovaConfig>,
    project: ProjectId,
    registry: ExtensionRegistry<DB>,
}

impl<DB: ?Sized + Send + Sync + 'static> IdeExtensions<DB> {
    pub fn new(db: Arc<DB>, config: Arc<NovaConfig>, project: ProjectId) -> Self {
        Self {
            db,
            config,
            project,
            registry: ExtensionRegistry::default(),
        }
    }

    pub fn registry(&self) -> &ExtensionRegistry<DB> {
        &self.registry
    }

    pub fn registry_mut(&mut self) -> &mut ExtensionRegistry<DB> {
        &mut self.registry
    }

    pub fn diagnostics(&self, cancel: CancellationToken, file: nova_ext::FileId) -> Vec<Diagnostic> {
        let ctx = ExtensionContext::new(
            Arc::clone(&self.db),
            Arc::clone(&self.config),
            self.project,
            cancel,
        );
        self.registry.diagnostics(ctx, DiagnosticParams { file })
    }

    pub fn completions(
        &self,
        cancel: CancellationToken,
        file: nova_ext::FileId,
        offset: usize,
    ) -> Vec<CompletionItem> {
        let ctx = ExtensionContext::new(
            Arc::clone(&self.db),
            Arc::clone(&self.config),
            self.project,
            cancel,
        );
        self.registry
            .completions(ctx, CompletionParams { file, offset })
    }

    pub fn code_actions(
        &self,
        cancel: CancellationToken,
        file: nova_ext::FileId,
        span: Option<Span>,
    ) -> Vec<CodeAction> {
        let ctx = ExtensionContext::new(
            Arc::clone(&self.db),
            Arc::clone(&self.config),
            self.project,
            cancel,
        );
        self.registry
            .code_actions(ctx, CodeActionParams { file, span })
    }

    pub fn navigation(&self, cancel: CancellationToken, symbol: Symbol) -> Vec<NavigationTarget> {
        let ctx = ExtensionContext::new(
            Arc::clone(&self.db),
            Arc::clone(&self.config),
            self.project,
            cancel,
        );
        self.registry
            .navigation(ctx, NavigationParams { symbol })
    }

    pub fn inlay_hints(&self, cancel: CancellationToken, file: nova_ext::FileId) -> Vec<InlayHint> {
        let ctx = ExtensionContext::new(
            Arc::clone(&self.db),
            Arc::clone(&self.config),
            self.project,
            cancel,
        );
        self.registry.inlay_hints(ctx, InlayHintParams { file })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nova_ext::{CodeActionProvider, CompletionProvider, DiagnosticProvider};
    use nova_framework::{Database, FrameworkAnalyzer, FrameworkAnalyzerAdapter, MemoryDatabase};

    struct FrameworkTestAnalyzer;

    impl FrameworkAnalyzer for FrameworkTestAnalyzer {
        fn applies_to(&self, _db: &dyn Database, _project: ProjectId) -> bool {
            true
        }

        fn diagnostics(&self, _db: &dyn Database, _file: nova_ext::FileId) -> Vec<Diagnostic> {
            vec![Diagnostic::warning("FW", "framework", Some(Span::new(0, 1)))]
        }

        fn completions(
            &self,
            _db: &dyn Database,
            _ctx: &nova_framework::CompletionContext,
        ) -> Vec<CompletionItem> {
            vec![CompletionItem::new("frameworkCompletion")]
        }
    }

    struct ExtraDiagProvider;
    impl DiagnosticProvider<dyn Database + Send + Sync> for ExtraDiagProvider {
        fn id(&self) -> &str {
            "extra.diag"
        }

        fn provide_diagnostics(
            &self,
            _ctx: ExtensionContext<dyn Database + Send + Sync>,
            _params: DiagnosticParams,
        ) -> Vec<Diagnostic> {
            vec![Diagnostic::warning("EXTRA", "extra", Some(Span::new(1, 2)))]
        }
    }

    struct ExtraActionProvider;
    impl CodeActionProvider<dyn Database + Send + Sync> for ExtraActionProvider {
        fn id(&self) -> &str {
            "extra.actions"
        }

        fn provide_code_actions(
            &self,
            _ctx: ExtensionContext<dyn Database + Send + Sync>,
            _params: CodeActionParams,
        ) -> Vec<CodeAction> {
            vec![CodeAction {
                title: "extra action".to_string(),
                kind: Some("quickfix".to_string()),
            }]
        }
    }

    struct ExtraCompletionProvider;
    impl CompletionProvider<dyn Database + Send + Sync> for ExtraCompletionProvider {
        fn id(&self) -> &str {
            "extra.completion"
        }

        fn provide_completions(
            &self,
            _ctx: ExtensionContext<dyn Database + Send + Sync>,
            _params: CompletionParams,
        ) -> Vec<CompletionItem> {
            vec![CompletionItem::new("extraCompletion")]
        }
    }

    #[test]
    fn aggregates_multiple_providers_including_framework_analyzer() {
        let mut db = MemoryDatabase::new();
        let project = db.add_project();
        let file = db.add_file(project);

        let db: Arc<dyn Database + Send + Sync> = Arc::new(db);
        let mut ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), project);

        let analyzer = FrameworkAnalyzerAdapter::new("framework.test", FrameworkTestAnalyzer).into_arc();
        ide.registry_mut()
            .register_diagnostic_provider(analyzer.clone())
            .unwrap();
        ide.registry_mut()
            .register_completion_provider(analyzer.clone())
            .unwrap();

        ide.registry_mut()
            .register_diagnostic_provider(Arc::new(ExtraDiagProvider))
            .unwrap();
        ide.registry_mut()
            .register_completion_provider(Arc::new(ExtraCompletionProvider))
            .unwrap();
        ide.registry_mut()
            .register_code_action_provider(Arc::new(ExtraActionProvider))
            .unwrap();

        let diags = ide.diagnostics(CancellationToken::new(), file);
        assert_eq!(diags.len(), 2);
        assert!(diags.iter().any(|d| d.message == "framework"));
        assert!(diags.iter().any(|d| d.message == "extra"));

        let completions = ide.completions(CancellationToken::new(), file, 0);
        assert_eq!(completions.len(), 2);
        assert!(completions.iter().any(|c| c.label == "frameworkCompletion"));
        assert!(completions.iter().any(|c| c.label == "extraCompletion"));

        let actions = ide.code_actions(CancellationToken::new(), file, None);
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].title, "extra action");
    }
}
