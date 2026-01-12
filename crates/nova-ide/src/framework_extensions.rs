use std::sync::Arc;

use nova_ext::{
    CompletionItem, CompletionParams, CompletionProvider, Diagnostic, DiagnosticParams,
    DiagnosticProvider, ExtensionContext, InlayHint, InlayHintParams, InlayHintProvider,
    NavigationParams, NavigationProvider, NavigationTarget, Symbol,
};
use nova_framework::{
    CompletionContext as FrameworkCompletionContext, FrameworkAnalyzer, Symbol as FrameworkSymbol,
};

/// Adapter that exposes a `nova-framework` [`FrameworkAnalyzer`] as `nova-ext` providers over a
/// text-only [`nova_db::Database`].
///
/// This is primarily intended for registering framework analyzers as **individual** providers so
/// `nova-ext` can apply per-provider circuit breakers, timeouts, and metrics.
pub struct FrameworkAnalyzerAdapterOnTextDb<A> {
    id: String,
    analyzer: A,
}

impl<A> FrameworkAnalyzerAdapterOnTextDb<A> {
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

impl<A> DiagnosticProvider<dyn nova_db::Database + Send + Sync>
    for FrameworkAnalyzerAdapterOnTextDb<A>
where
    A: FrameworkAnalyzer + Send + Sync + 'static,
{
    fn id(&self) -> &str {
        &self.id
    }

    fn provide_diagnostics(
        &self,
        ctx: ExtensionContext<dyn nova_db::Database + Send + Sync>,
        params: DiagnosticParams,
    ) -> Vec<Diagnostic> {
        if ctx.cancel.is_cancelled() {
            return Vec::new();
        }

        let Some(fw_db) = crate::framework_db::framework_db_for_file(
            Arc::clone(&ctx.db),
            params.file,
            &ctx.cancel,
        ) else {
            return Vec::new();
        };
        let project = fw_db.project_of_file(params.file);

        if !self.analyzer.applies_to(fw_db.as_ref(), project) {
            return Vec::new();
        }

        self.analyzer.diagnostics(fw_db.as_ref(), params.file)
    }
}

impl<A> CompletionProvider<dyn nova_db::Database + Send + Sync>
    for FrameworkAnalyzerAdapterOnTextDb<A>
where
    A: FrameworkAnalyzer + Send + Sync + 'static,
{
    fn id(&self) -> &str {
        &self.id
    }

    fn provide_completions(
        &self,
        ctx: ExtensionContext<dyn nova_db::Database + Send + Sync>,
        params: CompletionParams,
    ) -> Vec<CompletionItem> {
        if ctx.cancel.is_cancelled() {
            return Vec::new();
        }

        let Some(fw_db) = crate::framework_db::framework_db_for_file(
            Arc::clone(&ctx.db),
            params.file,
            &ctx.cancel,
        ) else {
            return Vec::new();
        };
        let project = fw_db.project_of_file(params.file);

        if !self.analyzer.applies_to(fw_db.as_ref(), project) {
            return Vec::new();
        }

        let completion_ctx = FrameworkCompletionContext {
            project,
            file: params.file,
            offset: params.offset,
        };
        self.analyzer.completions(fw_db.as_ref(), &completion_ctx)
    }
}

impl<A> NavigationProvider<dyn nova_db::Database + Send + Sync>
    for FrameworkAnalyzerAdapterOnTextDb<A>
where
    A: FrameworkAnalyzer + Send + Sync + 'static,
{
    fn id(&self) -> &str {
        &self.id
    }

    fn provide_navigation(
        &self,
        ctx: ExtensionContext<dyn nova_db::Database + Send + Sync>,
        params: NavigationParams,
    ) -> Vec<NavigationTarget> {
        if ctx.cancel.is_cancelled() {
            return Vec::new();
        }

        let (fw_db, project) = match params.symbol {
            Symbol::File(file) => {
                let Some(fw_db) = crate::framework_db::framework_db_for_file(
                    Arc::clone(&ctx.db),
                    file,
                    &ctx.cancel,
                ) else {
                    return Vec::new();
                };
                let project = fw_db.project_of_file(file);
                (fw_db, project)
            }
            Symbol::Class(class) => {
                // Best-effort: `nova_db::Database` does not currently expose a class -> file mapping,
                // so we pick an arbitrary file to anchor the project. Framework analyzers should be
                // tolerant of missing project-wide information.
                let Some(file) = ctx.db.all_file_ids().into_iter().next() else {
                    return Vec::new();
                };
                let Some(fw_db) = crate::framework_db::framework_db_for_file(
                    Arc::clone(&ctx.db),
                    file,
                    &ctx.cancel,
                ) else {
                    return Vec::new();
                };
                let project = fw_db.project_of_class(class);
                (fw_db, project)
            }
        };

        if !self.analyzer.applies_to(fw_db.as_ref(), project) {
            return Vec::new();
        }

        let symbol = match params.symbol {
            Symbol::File(file) => FrameworkSymbol::File(file),
            Symbol::Class(class) => FrameworkSymbol::Class(class),
        };

        self.analyzer
            .navigation(fw_db.as_ref(), &symbol)
            .into_iter()
            .map(|target| NavigationTarget {
                file: target.file,
                span: target.span,
                label: target.label,
            })
            .collect()
    }
}

impl<A> InlayHintProvider<dyn nova_db::Database + Send + Sync>
    for FrameworkAnalyzerAdapterOnTextDb<A>
where
    A: FrameworkAnalyzer + Send + Sync + 'static,
{
    fn id(&self) -> &str {
        &self.id
    }

    fn provide_inlay_hints(
        &self,
        ctx: ExtensionContext<dyn nova_db::Database + Send + Sync>,
        params: InlayHintParams,
    ) -> Vec<InlayHint> {
        if ctx.cancel.is_cancelled() {
            return Vec::new();
        }

        let Some(fw_db) = crate::framework_db::framework_db_for_file(
            Arc::clone(&ctx.db),
            params.file,
            &ctx.cancel,
        ) else {
            return Vec::new();
        };
        let project = fw_db.project_of_file(params.file);

        if !self.analyzer.applies_to(fw_db.as_ref(), project) {
            return Vec::new();
        }

        self.analyzer
            .inlay_hints(fw_db.as_ref(), params.file)
            .into_iter()
            .map(|hint| InlayHint {
                span: hint.span,
                label: hint.label,
            })
            .collect()
    }
}
