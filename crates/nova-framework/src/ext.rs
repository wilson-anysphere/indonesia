use crate::{CompletionContext, Database, FrameworkAnalyzer, InlayHint as FrameworkInlayHint, NavigationTarget as FrameworkNavigationTarget, Symbol as FrameworkSymbol};
use nova_ext::{
    CompletionParams, CompletionProvider, DiagnosticParams, DiagnosticProvider, ExtensionContext,
    InlayHint, InlayHintParams, InlayHintProvider, NavigationParams, NavigationProvider,
    NavigationTarget, Symbol,
};
use nova_types::{CompletionItem, Diagnostic};
use std::sync::Arc;

/// Adapter that exposes a `nova-framework` [`FrameworkAnalyzer`] via the unified `nova-ext` traits.
///
/// This allows existing framework analyzers (Lombok, Spring, etc.) to coexist with third-party
/// `nova-ext` providers without forcing an all-at-once migration.
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

impl<A> DiagnosticProvider<dyn Database + Send + Sync> for FrameworkAnalyzerAdapter<A>
where
    A: FrameworkAnalyzer + Send + Sync + 'static,
{
    fn id(&self) -> &str {
        &self.id
    }

    fn is_applicable(&self, ctx: &ExtensionContext<dyn Database + Send + Sync>) -> bool {
        self.analyzer.applies_to(ctx.db.as_ref(), ctx.project)
    }

    fn provide_diagnostics(
        &self,
        ctx: ExtensionContext<dyn Database + Send + Sync>,
        params: DiagnosticParams,
    ) -> Vec<Diagnostic> {
        self.analyzer.diagnostics(ctx.db.as_ref(), params.file)
    }
}

impl<A> CompletionProvider<dyn Database + Send + Sync> for FrameworkAnalyzerAdapter<A>
where
    A: FrameworkAnalyzer + Send + Sync + 'static,
{
    fn id(&self) -> &str {
        &self.id
    }

    fn is_applicable(&self, ctx: &ExtensionContext<dyn Database + Send + Sync>) -> bool {
        self.analyzer.applies_to(ctx.db.as_ref(), ctx.project)
    }

    fn provide_completions(
        &self,
        ctx: ExtensionContext<dyn Database + Send + Sync>,
        params: CompletionParams,
    ) -> Vec<CompletionItem> {
        let completion_ctx = CompletionContext {
            project: ctx.project,
            file: params.file,
            offset: params.offset,
        };
        self.analyzer.completions(ctx.db.as_ref(), &completion_ctx)
    }
}

impl<A> NavigationProvider<dyn Database + Send + Sync> for FrameworkAnalyzerAdapter<A>
where
    A: FrameworkAnalyzer + Send + Sync + 'static,
{
    fn id(&self) -> &str {
        &self.id
    }

    fn is_applicable(&self, ctx: &ExtensionContext<dyn Database + Send + Sync>) -> bool {
        self.analyzer.applies_to(ctx.db.as_ref(), ctx.project)
    }

    fn provide_navigation(
        &self,
        ctx: ExtensionContext<dyn Database + Send + Sync>,
        params: NavigationParams,
    ) -> Vec<NavigationTarget> {
        let symbol = match params.symbol {
            Symbol::File(file) => FrameworkSymbol::File(file),
            Symbol::Class(class) => FrameworkSymbol::Class(class),
        };

        self.analyzer
            .navigation(ctx.db.as_ref(), &symbol)
            .into_iter()
            .map(FrameworkNavigationTarget::into)
            .collect()
    }
}

impl<A> InlayHintProvider<dyn Database + Send + Sync> for FrameworkAnalyzerAdapter<A>
where
    A: FrameworkAnalyzer + Send + Sync + 'static,
{
    fn id(&self) -> &str {
        &self.id
    }

    fn is_applicable(&self, ctx: &ExtensionContext<dyn Database + Send + Sync>) -> bool {
        self.analyzer.applies_to(ctx.db.as_ref(), ctx.project)
    }

    fn provide_inlay_hints(
        &self,
        ctx: ExtensionContext<dyn Database + Send + Sync>,
        params: InlayHintParams,
    ) -> Vec<InlayHint> {
        self.analyzer
            .inlay_hints(ctx.db.as_ref(), params.file)
            .into_iter()
            .map(FrameworkInlayHint::into)
            .collect()
    }
}

impl From<FrameworkNavigationTarget> for NavigationTarget {
    fn from(value: FrameworkNavigationTarget) -> Self {
        Self {
            file: value.file,
            span: value.span,
            label: value.label,
        }
    }
}

impl From<FrameworkInlayHint> for InlayHint {
    fn from(value: FrameworkInlayHint) -> Self {
        Self {
            span: value.span,
            label: value.label,
        }
    }
}

