use nova_config::NovaConfig;
use nova_ext::{
    CodeAction, CodeActionParams, CompletionItem, CompletionParams, CompletionProvider, Diagnostic,
    DiagnosticParams, DiagnosticProvider, ExtensionContext, ExtensionRegistry, InlayHint,
    InlayHintParams, NavigationParams, NavigationTarget, ProjectId, Span, Symbol,
};
use nova_scheduler::CancellationToken;
use std::collections::HashSet;
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

    pub fn diagnostics(
        &self,
        cancel: CancellationToken,
        file: nova_ext::FileId,
    ) -> Vec<Diagnostic> {
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
        self.registry.navigation(ctx, NavigationParams { symbol })
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
    pub fn with_default_registry(db: Arc<DB>, config: Arc<NovaConfig>, project: ProjectId) -> Self {
        let mut this = Self::new(db, config, project);
        let registry = this.registry_mut();
        let _ = registry.register_diagnostic_provider(Arc::new(FrameworkDiagnosticProvider));
        let _ = registry.register_completion_provider(Arc::new(FrameworkCompletionProvider));
        this
    }
}

impl IdeExtensions<dyn nova_db::Database + Send + Sync> {
    pub fn with_default_registry(
        db: Arc<dyn nova_db::Database + Send + Sync>,
        config: Arc<NovaConfig>,
        project: ProjectId,
    ) -> Self {
        let mut this = Self::new(db, config, project);
        let registry = this.registry_mut();
        let _ = registry.register_diagnostic_provider(Arc::new(FrameworkDiagnosticProvider));
        let _ = registry.register_completion_provider(Arc::new(FrameworkCompletionProvider));
        this
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
        let mut diagnostics =
            crate::code_intelligence::core_file_diagnostics(self.db.as_ref(), file);
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
            crate::code_intelligence::core_completions(self.db.as_ref(), file, position);
        let text = self.db.file_content(file);
        let offset = crate::text::position_to_offset(text, position).unwrap_or(text.len());

        let extension_items = self
            .completions(cancel, file, offset)
            .into_iter()
            .map(|item| {
                let label = item.label;
                let mut out = lsp_types::CompletionItem {
                    label: label.clone(),
                    detail: item.detail,
                    ..lsp_types::CompletionItem::default()
                };

                if let Some(span) = item.replace_span {
                    out.text_edit =
                        Some(lsp_types::CompletionTextEdit::Edit(lsp_types::TextEdit {
                            range: crate::text::span_to_lsp_range(text, span),
                            new_text: label,
                        }));
                }

                out
            });

        completions.extend(extension_items);
        completions
    }

    /// Combine Nova's built-in refactor code actions with extension-provided code actions.
    pub fn code_actions_lsp(
        &self,
        cancel: CancellationToken,
        file: nova_ext::FileId,
        span: Option<Span>,
    ) -> Vec<lsp_types::CodeActionOrCommand> {
        let mut actions = Vec::new();

        let source = self.db.file_content(file);
        let uri = self
            .db
            .file_path(file)
            .and_then(|path| nova_core::AbsPathBuf::new(path.to_path_buf()).ok())
            .and_then(|path| nova_core::path_to_file_uri(&path).ok())
            .and_then(|uri| uri.parse().ok());

        if let (Some(uri), Some(span)) = (uri, span) {
            let selection = crate::text::span_to_lsp_range(source, span);

            actions.extend(crate::refactor::extract_member_code_actions(
                &uri, source, selection,
            ));

            if let Some(action) =
                crate::code_action::extract_method_code_action(source, uri.clone(), selection)
            {
                actions.push(lsp_types::CodeActionOrCommand::CodeAction(action));
            }

            actions.extend(crate::refactor::inline_method_code_actions(
                &uri,
                source,
                selection.start,
            ));
        }

        let extension_actions = self
            .code_actions(cancel, file, span)
            .into_iter()
            .map(|action| {
                let kind = action.kind.map(lsp_types::CodeActionKind::from);
                lsp_types::CodeActionOrCommand::CodeAction(lsp_types::CodeAction {
                    title: action.title,
                    kind,
                    ..lsp_types::CodeAction::default()
                })
            });
        actions.extend(extension_actions);

        actions
    }

    /// Combine Nova's built-in inlay hints with extension-provided inlay hints.
    pub fn inlay_hints_lsp(
        &self,
        cancel: CancellationToken,
        file: nova_ext::FileId,
        range: lsp_types::Range,
    ) -> Vec<lsp_types::InlayHint> {
        let text = self.db.file_content(file);
        let start_offset = crate::text::position_to_offset(text, range.start).unwrap_or(0);
        let end_offset = crate::text::position_to_offset(text, range.end).unwrap_or(text.len());

        let mut hints = crate::code_intelligence::inlay_hints(self.db.as_ref(), file, range);

        let mut seen: HashSet<(u32, u32, String)> = hints
            .iter()
            .map(|hint| {
                (
                    hint.position.line,
                    hint.position.character,
                    match &hint.label {
                        lsp_types::InlayHintLabel::String(label) => label.clone(),
                        lsp_types::InlayHintLabel::LabelParts(parts) => parts
                            .iter()
                            .map(|part| part.value.as_str())
                            .collect::<Vec<_>>()
                            .join(""),
                    },
                )
            })
            .collect();

        for hint in self.inlay_hints(cancel, file) {
            let Some(span) = hint.span else {
                continue;
            };

            if span.start < start_offset || span.start > end_offset {
                continue;
            }

            let position = crate::text::offset_to_position(text, span.start);
            let label = hint.label;
            let key = (position.line, position.character, label.clone());
            if !seen.insert(key) {
                continue;
            }

            hints.push(lsp_types::InlayHint {
                position,
                label: lsp_types::InlayHintLabel::String(label),
                kind: None,
                text_edits: None,
                tooltip: None,
                padding_left: None,
                padding_right: None,
                data: None,
            });
        }

        hints
    }
}

impl IdeExtensions<dyn nova_db::Database + Send + Sync> {
    /// Combine Nova's built-in diagnostics with registered extension diagnostics.
    pub fn all_diagnostics(
        &self,
        cancel: CancellationToken,
        file: nova_ext::FileId,
    ) -> Vec<Diagnostic> {
        let mut diagnostics =
            crate::code_intelligence::core_file_diagnostics(self.db.as_ref(), file);
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
            crate::code_intelligence::core_completions(self.db.as_ref(), file, position);
        let text = self.db.file_content(file);
        let offset = crate::text::position_to_offset(text, position).unwrap_or(text.len());

        let extension_items = self
            .completions(cancel, file, offset)
            .into_iter()
            .map(|item| {
                let label = item.label;
                let mut out = lsp_types::CompletionItem {
                    label: label.clone(),
                    detail: item.detail,
                    ..lsp_types::CompletionItem::default()
                };

                if let Some(span) = item.replace_span {
                    out.text_edit =
                        Some(lsp_types::CompletionTextEdit::Edit(lsp_types::TextEdit {
                            range: crate::text::span_to_lsp_range(text, span),
                            new_text: label,
                        }));
                }

                out
            });

        completions.extend(extension_items);
        completions
    }

    /// Combine Nova's built-in refactor code actions with extension-provided code actions.
    pub fn code_actions_lsp(
        &self,
        cancel: CancellationToken,
        file: nova_ext::FileId,
        span: Option<Span>,
    ) -> Vec<lsp_types::CodeActionOrCommand> {
        let mut actions = Vec::new();

        let source = self.db.file_content(file);
        let uri = self
            .db
            .file_path(file)
            .and_then(|path| nova_core::AbsPathBuf::new(path.to_path_buf()).ok())
            .and_then(|path| nova_core::path_to_file_uri(&path).ok())
            .and_then(|uri| uri.parse().ok());

        if let (Some(uri), Some(span)) = (uri, span) {
            let selection = crate::text::span_to_lsp_range(source, span);

            actions.extend(crate::refactor::extract_member_code_actions(
                &uri, source, selection,
            ));

            if let Some(action) =
                crate::code_action::extract_method_code_action(source, uri.clone(), selection)
            {
                actions.push(lsp_types::CodeActionOrCommand::CodeAction(action));
            }

            actions.extend(crate::refactor::inline_method_code_actions(
                &uri,
                source,
                selection.start,
            ));
        }

        let extension_actions = self
            .code_actions(cancel, file, span)
            .into_iter()
            .map(|action| {
                let kind = action.kind.map(lsp_types::CodeActionKind::from);
                lsp_types::CodeActionOrCommand::CodeAction(lsp_types::CodeAction {
                    title: action.title,
                    kind,
                    ..lsp_types::CodeAction::default()
                })
            });
        actions.extend(extension_actions);

        actions
    }

    /// Combine Nova's built-in inlay hints with extension-provided inlay hints.
    pub fn inlay_hints_lsp(
        &self,
        cancel: CancellationToken,
        file: nova_ext::FileId,
        range: lsp_types::Range,
    ) -> Vec<lsp_types::InlayHint> {
        let text = self.db.file_content(file);
        let start_offset = crate::text::position_to_offset(text, range.start).unwrap_or(0);
        let end_offset = crate::text::position_to_offset(text, range.end).unwrap_or(text.len());

        let mut hints = crate::code_intelligence::inlay_hints(self.db.as_ref(), file, range);

        let mut seen: HashSet<(u32, u32, String)> = hints
            .iter()
            .map(|hint| {
                (
                    hint.position.line,
                    hint.position.character,
                    match &hint.label {
                        lsp_types::InlayHintLabel::String(label) => label.clone(),
                        lsp_types::InlayHintLabel::LabelParts(parts) => parts
                            .iter()
                            .map(|part| part.value.as_str())
                            .collect::<Vec<_>>()
                            .join(""),
                    },
                )
            })
            .collect();

        for hint in self.inlay_hints(cancel, file) {
            let Some(span) = hint.span else {
                continue;
            };

            if span.start < start_offset || span.start > end_offset {
                continue;
            }

            let position = crate::text::offset_to_position(text, span.start);
            let label = hint.label;
            let key = (position.line, position.character, label.clone());
            if !seen.insert(key) {
                continue;
            }

            hints.push(lsp_types::InlayHint {
                position,
                label: lsp_types::InlayHintLabel::String(label),
                kind: None,
                text_edits: None,
                tooltip: None,
                padding_left: None,
                padding_right: None,
                data: None,
            });
        }

        hints
    }
}

struct FrameworkDiagnosticProvider;

impl<DB> DiagnosticProvider<DB> for FrameworkDiagnosticProvider
where
    DB: Send + Sync + 'static + nova_db::Database,
{
    fn id(&self) -> &str {
        "nova.framework.diagnostics"
    }

    fn provide_diagnostics(
        &self,
        ctx: ExtensionContext<DB>,
        params: DiagnosticParams,
    ) -> Vec<Diagnostic> {
        crate::framework_cache::framework_diagnostics(ctx.db.as_ref(), params.file, &ctx.cancel)
    }
}

impl DiagnosticProvider<dyn nova_db::Database + Send + Sync> for FrameworkDiagnosticProvider {
    fn id(&self) -> &str {
        "nova.framework.diagnostics"
    }

    fn provide_diagnostics(
        &self,
        ctx: ExtensionContext<dyn nova_db::Database + Send + Sync>,
        params: DiagnosticParams,
    ) -> Vec<Diagnostic> {
        crate::framework_cache::framework_diagnostics(ctx.db.as_ref(), params.file, &ctx.cancel)
    }
}

struct FrameworkCompletionProvider;

impl<DB> CompletionProvider<DB> for FrameworkCompletionProvider
where
    DB: Send + Sync + 'static + nova_db::Database,
{
    fn id(&self) -> &str {
        "nova.framework.completions"
    }

    fn provide_completions(
        &self,
        ctx: ExtensionContext<DB>,
        params: CompletionParams,
    ) -> Vec<CompletionItem> {
        crate::framework_cache::framework_completions(
            ctx.db.as_ref(),
            params.file,
            params.offset,
            &ctx.cancel,
        )
    }
}

impl CompletionProvider<dyn nova_db::Database + Send + Sync> for FrameworkCompletionProvider {
    fn id(&self) -> &str {
        "nova.framework.completions"
    }

    fn provide_completions(
        &self,
        ctx: ExtensionContext<dyn nova_db::Database + Send + Sync>,
        params: CompletionParams,
    ) -> Vec<CompletionItem> {
        crate::framework_cache::framework_completions(
            ctx.db.as_ref(),
            params.file,
            params.offset,
            &ctx.cancel,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nova_ext::{CodeActionProvider, CompletionProvider, DiagnosticProvider, InlayHintProvider};
    use nova_framework::{Database, FrameworkAnalyzer, FrameworkAnalyzerAdapter, MemoryDatabase};
    use std::path::PathBuf;

    struct FrameworkTestAnalyzer;

    impl FrameworkAnalyzer for FrameworkTestAnalyzer {
        fn applies_to(&self, _db: &dyn Database, _project: ProjectId) -> bool {
            true
        }

        fn diagnostics(&self, _db: &dyn Database, _file: nova_ext::FileId) -> Vec<Diagnostic> {
            vec![Diagnostic::warning(
                "FW",
                "framework",
                Some(Span::new(0, 1)),
            )]
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

        let analyzer =
            FrameworkAnalyzerAdapter::new("framework.test", FrameworkTestAnalyzer).into_arc();
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
        use nova_db::InMemoryFileStore;

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
                vec![Diagnostic::warning(
                    "EXT",
                    "extension diagnostic",
                    Some(Span::new(0, 1)),
                )]
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

        let mut db = InMemoryFileStore::new();
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
        assert!(diags
            .iter()
            .any(|d| d.message.contains("Cannot resolve symbol 'baz'")));
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

    #[test]
    fn combines_builtin_and_extension_code_actions() {
        use nova_db::InMemoryFileStore;

        struct ExtraActionProvider;
        impl CodeActionProvider<dyn nova_db::Database + Send + Sync> for ExtraActionProvider {
            fn id(&self) -> &str {
                "extra.actions"
            }

            fn provide_code_actions(
                &self,
                _ctx: ExtensionContext<dyn nova_db::Database + Send + Sync>,
                _params: CodeActionParams,
            ) -> Vec<CodeAction> {
                vec![CodeAction {
                    title: "extra action".to_string(),
                    kind: Some("quickfix".to_string()),
                }]
            }
        }

        let mut db = InMemoryFileStore::new();
        let file = db.file_id_for_path(PathBuf::from("/actions.java"));
        let source = r#"
class A {
  void m() {
    int x = 1 + 2;
  }
}
"#;
        db.set_file_text(file, source.to_string());

        let selection_start = source.find("1 + 2").expect("selection start");
        let selection_end = selection_start + "1 + 2".len();
        let selection = Span::new(selection_start, selection_end);

        let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(db);
        let mut ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), ProjectId::new(0));
        ide.registry_mut()
            .register_code_action_provider(Arc::new(ExtraActionProvider))
            .unwrap();

        let actions = ide.code_actions_lsp(CancellationToken::new(), file, Some(selection));
        let titles: Vec<_> = actions
            .iter()
            .filter_map(|action| match action {
                lsp_types::CodeActionOrCommand::CodeAction(action) => Some(action.title.as_str()),
                lsp_types::CodeActionOrCommand::Command(command) => Some(command.title.as_str()),
            })
            .collect();

        assert!(
            titles.iter().any(|t| t.contains("Extract constant")),
            "expected built-in extract constant action; got {titles:?}"
        );
        assert!(
            titles.iter().any(|t| *t == "extra action"),
            "expected extension action; got {titles:?}"
        );
    }

    #[test]
    fn combines_builtin_and_extension_inlay_hints() {
        use nova_db::InMemoryFileStore;

        struct ExtraInlayHintProvider;
        impl InlayHintProvider<dyn nova_db::Database + Send + Sync> for ExtraInlayHintProvider {
            fn id(&self) -> &str {
                "extra.inlay_hints"
            }

            fn provide_inlay_hints(
                &self,
                _ctx: ExtensionContext<dyn nova_db::Database + Send + Sync>,
                _params: InlayHintParams,
            ) -> Vec<InlayHint> {
                vec![InlayHint {
                    span: Some(Span::new(0, 1)),
                    label: "extension hint".to_string(),
                }]
            }
        }

        let mut db = InMemoryFileStore::new();
        let file = db.file_id_for_path(PathBuf::from("/inlay_hints.java"));
        let source = r#"
 class A {
   void m() {
     var x = "";
   }
 }
 "#
        .to_string();
        db.set_file_text(file, source.clone());

        let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(db);
        let mut ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), ProjectId::new(0));
        ide.registry_mut()
            .register_inlay_hint_provider(Arc::new(ExtraInlayHintProvider))
            .unwrap();

        let range = lsp_types::Range::new(
            lsp_types::Position::new(0, 0),
            crate::text::offset_to_position(&source, source.len()),
        );

        let hints = ide.inlay_hints_lsp(CancellationToken::new(), file, range);

        let labels: Vec<String> = hints
            .iter()
            .map(|hint| match &hint.label {
                lsp_types::InlayHintLabel::String(label) => label.clone(),
                lsp_types::InlayHintLabel::LabelParts(parts) => parts
                    .iter()
                    .map(|part| part.value.as_str())
                    .collect::<Vec<_>>()
                    .join(""),
            })
            .collect();

        assert!(
            labels.iter().any(|label| label == ": String"),
            "expected built-in var type hint; got {labels:?}"
        );
        assert!(
            labels.iter().any(|label| label == "extension hint"),
            "expected extension inlay hint; got {labels:?}"
        );

        let builtin_idx = labels
            .iter()
            .position(|label| label == ": String")
            .expect("missing built-in hint");
        let extension_idx = labels
            .iter()
            .position(|label| label == "extension hint")
            .expect("missing extension hint");
        assert!(
            builtin_idx < extension_idx,
            "expected built-in hints to come first; got {labels:?}"
        );
    }
}
