use nova_config::NovaConfig;
use nova_ext::{
    CodeAction, CodeActionParams, CompletionItem, CompletionParams, CompletionProvider, Diagnostic,
    DiagnosticParams, DiagnosticProvider, ExtensionContext, ExtensionRegistry, InlayHint,
    InlayHintParams, InlayHintProvider, NavigationParams, NavigationProvider, NavigationTarget,
    ProjectId, Span, Symbol,
};
use nova_framework::{
    AnalyzerRegistry,
    CompletionContext as FrameworkCompletionContext, Database as FrameworkDatabase, FrameworkAnalyzer,
    Symbol as FrameworkSymbol,
};
use nova_refactor::{
    organize_imports, workspace_edit_to_lsp, FileId as RefactorFileId, OrganizeImportsParams,
    TextDatabase,
};
use nova_scheduler::CancellationToken;
use std::collections::HashSet;
use std::sync::Arc;

use crate::text::TextIndex;

trait AsDynNovaDb {
    fn as_dyn_nova_db(&self) -> &dyn nova_db::Database;
}

impl<DB> AsDynNovaDb for DB
where
    DB: nova_db::Database,
{
    fn as_dyn_nova_db(&self) -> &dyn nova_db::Database {
        self
    }
}

impl AsDynNovaDb for dyn nova_db::Database + Send + Sync {
    fn as_dyn_nova_db(&self) -> &dyn nova_db::Database {
        self
    }
}

/// Adapter that exposes a `nova-framework` [`FrameworkAnalyzer`] via the unified `nova-ext` traits.
///
/// This allows framework analyzers (Lombok, Spring, etc.) to coexist with third-party `nova-ext`
/// providers without forcing an all-at-once migration.
pub struct FrameworkAnalyzerAdapter<A> {
    id: String,
    analyzer: A,
}

impl<A> FrameworkAnalyzerAdapter<A> {
    pub fn new(id: impl Into<String>, analyzer: A) -> Self {
        Self {
            id: id.into(),
            analyzer,
        }
    }

    pub fn into_arc(self) -> Arc<Self> {
        Arc::new(self)
    }
}

impl<A> DiagnosticProvider<dyn FrameworkDatabase + Send + Sync> for FrameworkAnalyzerAdapter<A>
where
    A: FrameworkAnalyzer + Send + Sync + 'static,
{
    fn id(&self) -> &str {
        &self.id
    }

    fn is_applicable(&self, ctx: &ExtensionContext<dyn FrameworkDatabase + Send + Sync>) -> bool {
        self.analyzer.applies_to(ctx.db.as_ref(), ctx.project)
    }

    fn provide_diagnostics(
        &self,
        ctx: ExtensionContext<dyn FrameworkDatabase + Send + Sync>,
        params: DiagnosticParams,
    ) -> Vec<Diagnostic> {
        self.analyzer
            .diagnostics_with_cancel(ctx.db.as_ref(), params.file, &ctx.cancel)
    }
}

impl<A> CompletionProvider<dyn FrameworkDatabase + Send + Sync> for FrameworkAnalyzerAdapter<A>
where
    A: FrameworkAnalyzer + Send + Sync + 'static,
{
    fn id(&self) -> &str {
        &self.id
    }

    fn is_applicable(&self, ctx: &ExtensionContext<dyn FrameworkDatabase + Send + Sync>) -> bool {
        self.analyzer.applies_to(ctx.db.as_ref(), ctx.project)
    }

    fn provide_completions(
        &self,
        ctx: ExtensionContext<dyn FrameworkDatabase + Send + Sync>,
        params: CompletionParams,
    ) -> Vec<CompletionItem> {
        let completion_ctx = FrameworkCompletionContext {
            project: ctx.project,
            file: params.file,
            offset: params.offset,
        };
        self.analyzer
            .completions_with_cancel(ctx.db.as_ref(), &completion_ctx, &ctx.cancel)
    }
}

impl<A> NavigationProvider<dyn FrameworkDatabase + Send + Sync> for FrameworkAnalyzerAdapter<A>
where
    A: FrameworkAnalyzer + Send + Sync + 'static,
{
    fn id(&self) -> &str {
        &self.id
    }

    fn is_applicable(&self, ctx: &ExtensionContext<dyn FrameworkDatabase + Send + Sync>) -> bool {
        self.analyzer.applies_to(ctx.db.as_ref(), ctx.project)
    }

    fn provide_navigation(
        &self,
        ctx: ExtensionContext<dyn FrameworkDatabase + Send + Sync>,
        params: NavigationParams,
    ) -> Vec<NavigationTarget> {
        let symbol = match params.symbol {
            Symbol::File(file) => FrameworkSymbol::File(file),
            Symbol::Class(class) => FrameworkSymbol::Class(class),
        };

        self.analyzer
            .navigation_with_cancel(ctx.db.as_ref(), &symbol, &ctx.cancel)
            .into_iter()
            .map(|target| NavigationTarget {
                file: target.file,
                span: target.span,
                label: target.label,
            })
            .collect()
    }
}

impl<A> InlayHintProvider<dyn FrameworkDatabase + Send + Sync> for FrameworkAnalyzerAdapter<A>
where
    A: FrameworkAnalyzer + Send + Sync + 'static,
{
    fn id(&self) -> &str {
        &self.id
    }

    fn is_applicable(&self, ctx: &ExtensionContext<dyn FrameworkDatabase + Send + Sync>) -> bool {
        self.analyzer.applies_to(ctx.db.as_ref(), ctx.project)
    }

    fn provide_inlay_hints(
        &self,
        ctx: ExtensionContext<dyn FrameworkDatabase + Send + Sync>,
        params: InlayHintParams,
    ) -> Vec<InlayHint> {
        self.analyzer
            .inlay_hints_with_cancel(ctx.db.as_ref(), params.file, &ctx.cancel)
            .into_iter()
            .map(|hint| InlayHint {
                span: hint.span,
                label: hint.label,
            })
            .collect()
    }
}

pub const FRAMEWORK_ANALYZER_REGISTRY_PROVIDER_ID: &str = "nova.framework.analyzer_registry";

/// `nova-ext` provider that runs a `nova_framework::AnalyzerRegistry` against a best-effort
/// `nova_framework::Database` adapter (see [`crate::framework_db`]).
pub struct FrameworkAnalyzerRegistryProvider {
    registry: Arc<AnalyzerRegistry>,
    fast_noop: bool,
}

impl FrameworkAnalyzerRegistryProvider {
    pub fn new(registry: Arc<AnalyzerRegistry>) -> Self {
        Self {
            registry,
            fast_noop: false,
        }
    }

    /// Construct a provider that always returns empty results without attempting to build the
    /// framework database.
    ///
    /// This is used by `IdeExtensions::with_default_registry` to register the provider ID in the
    /// registry (so downstream callers can opt into registry-backed analyzers) without adding per
    /// request overhead while Nova's legacy `framework_cache` providers are still the source of
    /// built-in framework intelligence.
    pub fn empty() -> Self {
        Self {
            registry: Arc::new(AnalyzerRegistry::new()),
            fast_noop: true,
        }
    }

    pub fn into_arc(self) -> Arc<Self> {
        Arc::new(self)
    }

    fn framework_db(
        &self,
        db: Arc<dyn nova_db::Database + Send + Sync>,
        file: nova_ext::FileId,
        cancel: &CancellationToken,
    ) -> Option<Arc<dyn nova_framework::Database + Send + Sync>> {
        if cancel.is_cancelled() {
            return None;
        }
        crate::framework_db::framework_db_for_file(db, file, cancel)
    }
}

impl<DB> DiagnosticProvider<DB> for FrameworkAnalyzerRegistryProvider
where
    DB: Send + Sync + 'static + nova_db::Database,
{
    fn id(&self) -> &str {
        FRAMEWORK_ANALYZER_REGISTRY_PROVIDER_ID
    }

    fn provide_diagnostics(
        &self,
        ctx: ExtensionContext<DB>,
        params: DiagnosticParams,
    ) -> Vec<Diagnostic> {
        if self.fast_noop || ctx.cancel.is_cancelled() {
            return Vec::new();
        }
        let host_db: Arc<dyn nova_db::Database + Send + Sync> = ctx.db.clone();
        let Some(fw_db) = self.framework_db(host_db, params.file, &ctx.cancel) else {
            return Vec::new();
        };
        self.registry.framework_diagnostics(fw_db.as_ref(), params.file)
    }
}

impl<DB> CompletionProvider<DB> for FrameworkAnalyzerRegistryProvider
where
    DB: Send + Sync + 'static + nova_db::Database,
{
    fn id(&self) -> &str {
        FRAMEWORK_ANALYZER_REGISTRY_PROVIDER_ID
    }

    fn provide_completions(
        &self,
        ctx: ExtensionContext<DB>,
        params: CompletionParams,
    ) -> Vec<CompletionItem> {
        if self.fast_noop || ctx.cancel.is_cancelled() {
            return Vec::new();
        }
        let host_db: Arc<dyn nova_db::Database + Send + Sync> = ctx.db.clone();
        let Some(fw_db) = self.framework_db(host_db, params.file, &ctx.cancel) else {
            return Vec::new();
        };

        let project = fw_db.project_of_file(params.file);
        let completion_ctx = FrameworkCompletionContext {
            project,
            file: params.file,
            offset: params.offset,
        };
        self.registry
            .framework_completions(fw_db.as_ref(), &completion_ctx)
    }
}

impl<DB> NavigationProvider<DB> for FrameworkAnalyzerRegistryProvider
where
    DB: Send + Sync + 'static + nova_db::Database,
{
    fn id(&self) -> &str {
        FRAMEWORK_ANALYZER_REGISTRY_PROVIDER_ID
    }

    fn provide_navigation(
        &self,
        ctx: ExtensionContext<DB>,
        params: NavigationParams,
    ) -> Vec<NavigationTarget> {
        if self.fast_noop || ctx.cancel.is_cancelled() {
            return Vec::new();
        }
        let file = match params.symbol {
            Symbol::File(file) => file,
            // `nova-ext` navigation currently doesn't provide a way to recover the owning file for a
            // `ClassId`, so we can't safely pick a root-scoped framework DB here.
            Symbol::Class(_) => return Vec::new(),
        };

        let host_db: Arc<dyn nova_db::Database + Send + Sync> = ctx.db.clone();
        let Some(fw_db) = self.framework_db(host_db, file, &ctx.cancel) else {
            return Vec::new();
        };

        let symbol = match params.symbol {
            Symbol::File(file) => FrameworkSymbol::File(file),
            Symbol::Class(class) => FrameworkSymbol::Class(class),
        };

        self.registry
            .framework_navigation_targets(fw_db.as_ref(), &symbol)
            .into_iter()
            .map(|target| NavigationTarget {
                file: target.file,
                span: target.span,
                label: target.label,
            })
            .collect()
    }
}

impl<DB> InlayHintProvider<DB> for FrameworkAnalyzerRegistryProvider
where
    DB: Send + Sync + 'static + nova_db::Database,
{
    fn id(&self) -> &str {
        FRAMEWORK_ANALYZER_REGISTRY_PROVIDER_ID
    }

    fn provide_inlay_hints(
        &self,
        ctx: ExtensionContext<DB>,
        params: InlayHintParams,
    ) -> Vec<InlayHint> {
        if self.fast_noop || ctx.cancel.is_cancelled() {
            return Vec::new();
        }
        let host_db: Arc<dyn nova_db::Database + Send + Sync> = ctx.db.clone();
        let Some(fw_db) = self.framework_db(host_db, params.file, &ctx.cancel) else {
            return Vec::new();
        };

        self.registry
            .framework_inlay_hints(fw_db.as_ref(), params.file)
            .into_iter()
            .map(|hint| InlayHint {
                span: hint.span,
                label: hint.label,
            })
            .collect()
    }
}

impl DiagnosticProvider<dyn nova_db::Database + Send + Sync> for FrameworkAnalyzerRegistryProvider {
    fn id(&self) -> &str {
        FRAMEWORK_ANALYZER_REGISTRY_PROVIDER_ID
    }

    fn provide_diagnostics(
        &self,
        ctx: ExtensionContext<dyn nova_db::Database + Send + Sync>,
        params: DiagnosticParams,
    ) -> Vec<Diagnostic> {
        if self.fast_noop || ctx.cancel.is_cancelled() {
            return Vec::new();
        }
        let Some(fw_db) = self.framework_db(ctx.db.clone(), params.file, &ctx.cancel) else {
            return Vec::new();
        };
        self.registry.framework_diagnostics(fw_db.as_ref(), params.file)
    }
}

impl CompletionProvider<dyn nova_db::Database + Send + Sync> for FrameworkAnalyzerRegistryProvider {
    fn id(&self) -> &str {
        FRAMEWORK_ANALYZER_REGISTRY_PROVIDER_ID
    }

    fn provide_completions(
        &self,
        ctx: ExtensionContext<dyn nova_db::Database + Send + Sync>,
        params: CompletionParams,
    ) -> Vec<CompletionItem> {
        if self.fast_noop || ctx.cancel.is_cancelled() {
            return Vec::new();
        }
        let Some(fw_db) = self.framework_db(ctx.db.clone(), params.file, &ctx.cancel) else {
            return Vec::new();
        };

        let project = fw_db.project_of_file(params.file);
        let completion_ctx = FrameworkCompletionContext {
            project,
            file: params.file,
            offset: params.offset,
        };
        self.registry
            .framework_completions(fw_db.as_ref(), &completion_ctx)
    }
}

impl NavigationProvider<dyn nova_db::Database + Send + Sync> for FrameworkAnalyzerRegistryProvider {
    fn id(&self) -> &str {
        FRAMEWORK_ANALYZER_REGISTRY_PROVIDER_ID
    }

    fn provide_navigation(
        &self,
        ctx: ExtensionContext<dyn nova_db::Database + Send + Sync>,
        params: NavigationParams,
    ) -> Vec<NavigationTarget> {
        if self.fast_noop || ctx.cancel.is_cancelled() {
            return Vec::new();
        }
        let file = match params.symbol {
            Symbol::File(file) => file,
            Symbol::Class(_) => return Vec::new(),
        };

        let Some(fw_db) = self.framework_db(ctx.db.clone(), file, &ctx.cancel) else {
            return Vec::new();
        };

        let symbol = match params.symbol {
            Symbol::File(file) => FrameworkSymbol::File(file),
            Symbol::Class(class) => FrameworkSymbol::Class(class),
        };

        self.registry
            .framework_navigation_targets(fw_db.as_ref(), &symbol)
            .into_iter()
            .map(|target| NavigationTarget {
                file: target.file,
                span: target.span,
                label: target.label,
            })
            .collect()
    }
}

impl InlayHintProvider<dyn nova_db::Database + Send + Sync> for FrameworkAnalyzerRegistryProvider {
    fn id(&self) -> &str {
        FRAMEWORK_ANALYZER_REGISTRY_PROVIDER_ID
    }

    fn provide_inlay_hints(
        &self,
        ctx: ExtensionContext<dyn nova_db::Database + Send + Sync>,
        params: InlayHintParams,
    ) -> Vec<InlayHint> {
        if self.fast_noop || ctx.cancel.is_cancelled() {
            return Vec::new();
        }
        let Some(fw_db) = self.framework_db(ctx.db.clone(), params.file, &ctx.cancel) else {
            return Vec::new();
        };

        self.registry
            .framework_inlay_hints(fw_db.as_ref(), params.file)
            .into_iter()
            .map(|hint| InlayHint {
                span: hint.span,
                label: hint.label,
            })
            .collect()
    }
}
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

    pub fn with_registry(
        db: Arc<DB>,
        config: Arc<NovaConfig>,
        project: ProjectId,
        registry: ExtensionRegistry<DB>,
    ) -> Self {
        Self {
            db,
            config,
            project,
            registry,
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

#[allow(private_bounds)]
impl<DB: ?Sized> IdeExtensions<DB>
where
    DB: Send + Sync + 'static + nova_db::Database + AsDynNovaDb,
{
    pub fn with_default_registry(db: Arc<DB>, config: Arc<NovaConfig>, project: ProjectId) -> Self {
        let mut this = Self::new(db, config, project);
        let registry = this.registry_mut();
        let _ = registry.register_diagnostic_provider(Arc::new(FrameworkDiagnosticProvider));
        let _ = registry.register_completion_provider(Arc::new(FrameworkCompletionProvider));

        let provider = FrameworkAnalyzerRegistryProvider::empty().into_arc();
        let _ = registry.register_diagnostic_provider(provider.clone());
        let _ = registry.register_completion_provider(provider);
        this
    }
}

#[allow(private_bounds)]
impl<DB: ?Sized> IdeExtensions<DB>
where
    DB: Send + Sync + 'static + nova_db::Database + AsDynNovaDb,
{
    /// Combine Nova's built-in diagnostics with registered extension diagnostics.
    pub fn all_diagnostics(
        &self,
        cancel: CancellationToken,
        file: nova_ext::FileId,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = crate::code_intelligence::core_file_diagnostics(
            self.db.as_ref().as_dyn_nova_db(),
            file,
        );
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
        let mut completions = crate::code_intelligence::core_completions(
            self.db.as_ref().as_dyn_nova_db(),
            file,
            position,
        );
        let text = self.db.file_content(file);
        let text_index = TextIndex::new(text);
        let offset = text_index.position_to_offset(position).unwrap_or(text.len());

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
                            range: text_index.span_to_lsp_range(span),
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
        let uri: Option<lsp_types::Uri> = self
            .db
            .file_path(file)
            .and_then(|path| nova_core::AbsPathBuf::new(path.to_path_buf()).ok())
            .and_then(|path| nova_core::path_to_file_uri(&path).ok())
            .and_then(|uri| uri.parse().ok());

        if let Some(uri) = uri.clone() {
            if source.contains("import") {
                let file = RefactorFileId::new(uri.to_string());
                let db = TextDatabase::new([(file.clone(), source.to_string())]);
                if let Ok(edit) = organize_imports(&db, OrganizeImportsParams { file: file.clone() })
                {
                    if !edit.is_empty() {
                        if let Ok(lsp_edit) = workspace_edit_to_lsp(&db, &edit) {
                            actions.push(lsp_types::CodeActionOrCommand::CodeAction(
                                lsp_types::CodeAction {
                                    title: "Organize imports".to_string(),
                                    kind: Some(
                                        lsp_types::CodeActionKind::SOURCE_ORGANIZE_IMPORTS,
                                    ),
                                    edit: Some(lsp_edit),
                                    ..lsp_types::CodeAction::default()
                                },
                            ));
                        }
                    }
                }
            }
        }

        if let (Some(uri), Some(span)) = (uri, span) {
            let source_index = TextIndex::new(source);
            let selection = source_index.span_to_lsp_range(span);

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
        let text_index = TextIndex::new(text);
        let start_offset = text_index.position_to_offset(range.start).unwrap_or(0);
        let end_offset = text_index.position_to_offset(range.end).unwrap_or(text.len());

        let mut hints =
            crate::code_intelligence::inlay_hints(self.db.as_ref().as_dyn_nova_db(), file, range);

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

            let position = text_index.offset_to_position(span.start);
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

impl<DB: ?Sized> DiagnosticProvider<DB> for FrameworkDiagnosticProvider
where
    DB: Send + Sync + 'static + nova_db::Database + AsDynNovaDb,
{
    fn id(&self) -> &str {
        "nova.framework.diagnostics"
    }

    fn provide_diagnostics(
        &self,
        ctx: ExtensionContext<DB>,
        params: DiagnosticParams,
    ) -> Vec<Diagnostic> {
        crate::framework_cache::framework_diagnostics(
            ctx.db.as_ref().as_dyn_nova_db(),
            params.file,
            &ctx.cancel,
        )
    }
}

struct FrameworkCompletionProvider;

impl<DB: ?Sized> CompletionProvider<DB> for FrameworkCompletionProvider
where
    DB: Send + Sync + 'static + nova_db::Database + AsDynNovaDb,
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
            ctx.db.as_ref().as_dyn_nova_db(),
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
    use nova_framework::{Database, FrameworkAnalyzer, MemoryDatabase};
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

    struct CancellationAwareAnalyzer;
    impl FrameworkAnalyzer for CancellationAwareAnalyzer {
        fn applies_to(&self, _db: &dyn Database, _project: ProjectId) -> bool {
            true
        }

        fn diagnostics_with_cancel(
            &self,
            _db: &dyn Database,
            _file: nova_ext::FileId,
            cancel: &CancellationToken,
        ) -> Vec<Diagnostic> {
            if cancel.is_cancelled() {
                Vec::new()
            } else {
                vec![Diagnostic::warning(
                    "FW",
                    "framework",
                    Some(Span::new(0, 1)),
                )]
            }
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
    fn framework_analyzer_adapter_propagates_cancellation() {
        let mut db = MemoryDatabase::new();
        let project = db.add_project();
        let file = db.add_file(project);

        let db: Arc<dyn Database + Send + Sync> = Arc::new(db);
        let mut ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), project);

        let analyzer = FrameworkAnalyzerAdapter::new("framework.cancel", CancellationAwareAnalyzer)
            .into_arc();
        ide.registry_mut()
            .register_diagnostic_provider(analyzer)
            .unwrap();

        let diags = ide.diagnostics(CancellationToken::new(), file);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].message, "framework");

        let cancel = CancellationToken::new();
        cancel.cancel();
        let diags = ide.diagnostics(cancel, file);
        assert!(diags.is_empty());
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
