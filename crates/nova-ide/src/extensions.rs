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

    pub fn db(&self) -> &Arc<DB> {
        &self.db
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

impl<DB> IdeExtensions<DB>
where
    DB: Send + Sync + 'static + nova_db::Database,
{
    /// Combine Nova's built-in diagnostics with registered extension diagnostics.
    pub fn all_diagnostics(
        &self,
        cancel: CancellationToken,
        file: nova_ext::FileId,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = crate::code_intelligence::file_diagnostics(self.db.as_ref(), file);
        diagnostics.extend(self.diagnostics(cancel, file));
        diagnostics
    }

    /// Combine Nova's built-in completion items with extension-provided completion items.
    pub fn completions_lsp(
        &self,
        cancel: CancellationToken,
        file: nova_ext::FileId,
        position: lsp_types::Position,
    ) -> Vec<lsp_types::CompletionItem> {
        let mut completions =
            crate::code_intelligence::completions(self.db.as_ref(), file, position);
        let text = self.db.file_content(file);
        let offset = crate::code_intelligence::position_to_offset(text, position);

        let extension_items = self.completions(cancel, file, offset).into_iter().map(|item| {
            lsp_types::CompletionItem {
                label: item.label,
                detail: item.detail,
                ..lsp_types::CompletionItem::default()
            }
        });

        completions.extend(extension_items);
        completions
    }
}

impl IdeExtensions<dyn nova_db::Database + Send + Sync> {
    /// Combine Nova's built-in diagnostics with registered extension diagnostics.
    pub fn all_diagnostics(
        &self,
        cancel: CancellationToken,
        file: nova_ext::FileId,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = crate::code_intelligence::file_diagnostics(self.db.as_ref(), file);
        diagnostics.extend(self.diagnostics(cancel, file));
        diagnostics
    }

    /// Combine Nova's built-in completion items with extension-provided completion items.
    pub fn completions_lsp(
        &self,
        cancel: CancellationToken,
        file: nova_ext::FileId,
        position: lsp_types::Position,
    ) -> Vec<lsp_types::CompletionItem> {
        let mut completions =
            crate::code_intelligence::completions(self.db.as_ref(), file, position);
        let text = self.db.file_content(file);
        let offset = crate::code_intelligence::position_to_offset(text, position);

        let extension_items = self.completions(cancel, file, offset).into_iter().map(|item| {
            lsp_types::CompletionItem {
                label: item.label,
                detail: item.detail,
                ..lsp_types::CompletionItem::default()
            }
        });

        completions.extend(extension_items);
        completions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nova_ext::{CodeActionProvider, CompletionProvider, DiagnosticProvider};
    use nova_framework::{Database, FrameworkAnalyzer, FrameworkAnalyzerAdapter, MemoryDatabase};
    use std::path::PathBuf;

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

    #[test]
    fn combines_builtin_and_extension_diagnostics_and_completions() {
        use nova_db::RootDatabase;

        struct ExtraDiagProvider;
        impl DiagnosticProvider<dyn nova_db::Database + Send + Sync> for ExtraDiagProvider {
            fn id(&self) -> &str {
                "extra.diag"
            }

            fn provide_diagnostics(
                &self,
                _ctx: ExtensionContext<dyn nova_db::Database + Send + Sync>,
                _params: DiagnosticParams,
            ) -> Vec<Diagnostic> {
                vec![Diagnostic::warning("EXT", "extension diagnostic", Some(Span::new(0, 1)))]
            }
        }

        struct ExtraCompletionProvider;
        impl CompletionProvider<dyn nova_db::Database + Send + Sync> for ExtraCompletionProvider {
            fn id(&self) -> &str {
                "extra.completion"
            }

            fn provide_completions(
                &self,
                _ctx: ExtensionContext<dyn nova_db::Database + Send + Sync>,
                _params: CompletionParams,
            ) -> Vec<CompletionItem> {
                vec![CompletionItem::new("extraCompletion")]
            }
        }

        let mut db = RootDatabase::new();
        let diagnostics_file = db.file_id_for_path(PathBuf::from("/diagnostics.java"));
        db.set_file_text(
            diagnostics_file,
            r#"
class A {
  void m() {
    baz();
  }
}
"#
            .to_string(),
        );

        let completion_file = db.file_id_for_path(PathBuf::from("/completion.java"));
        db.set_file_text(
            completion_file,
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
        let mut ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), ProjectId::new(0));
        ide.registry_mut()
            .register_diagnostic_provider(Arc::new(ExtraDiagProvider))
            .unwrap();
        ide.registry_mut()
            .register_completion_provider(Arc::new(ExtraCompletionProvider))
            .unwrap();

        let diags = ide.all_diagnostics(CancellationToken::new(), diagnostics_file);
        assert!(diags.iter().any(|d| d.message.contains("Cannot resolve symbol 'baz'")));
        assert!(diags.iter().any(|d| d.message == "extension diagnostic"));

        // `s.` is on line 4 (0-based; account for leading newline in fixture).
        let completions = ide.completions_lsp(
            CancellationToken::new(),
            completion_file,
            lsp_types::Position::new(4, 6),
        );
        let labels: Vec<_> = completions.iter().map(|c| c.label.as_str()).collect();
        assert!(labels.contains(&"length"));
        assert!(labels.contains(&"extraCompletion"));
    }
}
