use nova_config::NovaConfig;
use nova_ext::{
    CodeAction, CodeActionParams, CompletionItem, CompletionParams, CompletionProvider, Diagnostic,
    DiagnosticParams, DiagnosticProvider, ExtensionContext, ExtensionRegistry, InlayHint,
    InlayHintParams, InlayHintProvider, NavigationParams, NavigationProvider, NavigationTarget,
    ProjectId, Span, Symbol,
};
use nova_framework::{
    AnalyzerRegistry, CompletionContext as FrameworkCompletionContext,
    Database as FrameworkDatabase, FrameworkAnalyzer, Symbol as FrameworkSymbol,
};
use nova_refactor::{
    organize_imports, workspace_edit_to_lsp, FileId as RefactorFileId, OrganizeImportsParams,
    TextDatabase,
};
use nova_scheduler::CancellationToken;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

pub use crate::framework_db_adapter::FrameworkIdeDatabase;
use crate::text::TextIndex;

/// Combined database surface used by `nova-lsp` for extension execution.
///
/// Rust only allows a single non-auto trait in a `dyn ...` trait object. `nova-lsp` needs a trait
/// object that supports both the legacy `nova-db` text queries and the newer `nova-framework`
/// analyzer query surface, so we introduce this named shim trait.
pub trait IdeFrameworkDatabase: nova_db::Database + FrameworkDatabase {}

impl<T> IdeFrameworkDatabase for T where T: nova_db::Database + FrameworkDatabase {}

trait AsDynNovaDb {
    fn as_dyn_nova_db(&self) -> &dyn nova_db::Database;

    fn into_dyn_nova_db(self: Arc<Self>) -> Arc<dyn nova_db::Database + Send + Sync>;
}

impl<DB> AsDynNovaDb for DB
where
    DB: nova_db::Database + Send + Sync + 'static,
{
    fn as_dyn_nova_db(&self) -> &dyn nova_db::Database {
        self
    }

    fn into_dyn_nova_db(self: Arc<Self>) -> Arc<dyn nova_db::Database + Send + Sync> {
        self
    }
}

impl AsDynNovaDb for dyn nova_db::Database + Send + Sync {
    fn as_dyn_nova_db(&self) -> &dyn nova_db::Database {
        self
    }

    fn into_dyn_nova_db(self: Arc<Self>) -> Arc<dyn nova_db::Database + Send + Sync> {
        self
    }
}

impl AsDynNovaDb for dyn IdeFrameworkDatabase + Send + Sync {
    fn as_dyn_nova_db(&self) -> &dyn nova_db::Database {
        self
    }

    fn into_dyn_nova_db(self: Arc<Self>) -> Arc<dyn nova_db::Database + Send + Sync> {
        self
    }
}

pub use crate::framework_extensions::FrameworkAnalyzerAdapterOnTextDb;

/// Adapter that exposes a `nova-framework` [`FrameworkAnalyzer`] via the unified `nova-ext` traits.
///
/// This allows framework analyzers (Lombok, Spring, etc.) to coexist with third-party `nova-ext`
/// providers without forcing an all-at-once migration.
pub struct FrameworkAnalyzerAdapter<A> {
    id: String,
    analyzer: A,
    applicability_cache: Mutex<HashMap<ProjectId, bool>>,
}

impl<A> FrameworkAnalyzerAdapter<A> {
    pub fn new(id: impl Into<String>, analyzer: A) -> Self {
        Self {
            id: id.into(),
            analyzer,
            applicability_cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn into_arc(self) -> Arc<Self> {
        Arc::new(self)
    }
}

impl<A> FrameworkAnalyzerAdapter<A>
where
    A: FrameworkAnalyzer,
{
    fn cached_applicability(&self, project: ProjectId) -> Option<bool> {
        self.applicability_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&project)
            .copied()
    }

    fn ensure_applicability<DB>(&self, ctx: &ExtensionContext<DB>) -> bool
    where
        DB: ?Sized + Send + Sync + FrameworkDatabase,
    {
        if let Some(value) = self.cached_applicability(ctx.project) {
            return value;
        }

        if ctx.cancel.is_cancelled() {
            return false;
        }

        let db = FrameworkDatabaseView(ctx.db.as_ref());
        let applicable = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.analyzer.applies_to(&db, ctx.project)
        }))
        .unwrap_or(false);

        self.applicability_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(ctx.project, applicable);

        applicable
    }
}

/// Adapter that exposes a single `nova-framework` [`FrameworkAnalyzer`] via the `nova-ext` traits
/// on the host database type (`dyn nova_db::Database`).
///
/// Unlike [`FrameworkAnalyzerRegistryProvider`] (which runs *all* analyzers behind a single provider
/// id), this adapter allows each framework analyzer to be registered as its own `nova-ext`
/// provider. That in turn enables per-analyzer timeouts/metrics/circuit-breaker isolation.
pub struct FrameworkAnalyzerOnTextDbAdapter<A> {
    id: String,
    analyzer: A,
}

impl<A> FrameworkAnalyzerOnTextDbAdapter<A> {
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

/// A thin wrapper that lets us pass a `&DB` to `FrameworkAnalyzer` APIs as a `&dyn Database`.
///
/// `FrameworkAnalyzer` is defined in terms of `&dyn nova_framework::Database`. Converting a `&DB`
/// into that trait object requires `DB: Sized`, which breaks when `DB` itself is a trait object
/// (e.g. `dyn nova_framework::Database + Send + Sync`, or a multi-trait object).
///
/// By wrapping the reference in a sized type that implements `nova_framework::Database` via
/// delegation, we can support both concrete database types and trait-object databases.
struct FrameworkDatabaseView<'a, DB: ?Sized + FrameworkDatabase>(&'a DB);

impl<DB: ?Sized + FrameworkDatabase> FrameworkDatabase for FrameworkDatabaseView<'_, DB> {
    fn class(&self, class: nova_types::ClassId) -> &nova_hir::framework::ClassData {
        FrameworkDatabase::class(self.0, class)
    }

    fn project_of_class(&self, class: nova_types::ClassId) -> nova_core::ProjectId {
        FrameworkDatabase::project_of_class(self.0, class)
    }

    fn project_of_file(&self, file: nova_core::FileId) -> nova_core::ProjectId {
        FrameworkDatabase::project_of_file(self.0, file)
    }

    fn file_text(&self, file: nova_core::FileId) -> Option<&str> {
        FrameworkDatabase::file_text(self.0, file)
    }

    fn file_path(&self, file: nova_core::FileId) -> Option<&std::path::Path> {
        FrameworkDatabase::file_path(self.0, file)
    }

    fn file_id(&self, path: &std::path::Path) -> Option<nova_core::FileId> {
        FrameworkDatabase::file_id(self.0, path)
    }

    fn all_files(&self, project: nova_core::ProjectId) -> Vec<nova_core::FileId> {
        FrameworkDatabase::all_files(self.0, project)
    }

    fn all_classes(&self, project: nova_core::ProjectId) -> Vec<nova_types::ClassId> {
        FrameworkDatabase::all_classes(self.0, project)
    }

    fn has_dependency(&self, project: nova_core::ProjectId, group: &str, artifact: &str) -> bool {
        FrameworkDatabase::has_dependency(self.0, project, group, artifact)
    }

    fn has_class_on_classpath(&self, project: nova_core::ProjectId, binary_name: &str) -> bool {
        FrameworkDatabase::has_class_on_classpath(self.0, project, binary_name)
    }

    fn has_class_on_classpath_prefix(&self, project: nova_core::ProjectId, prefix: &str) -> bool {
        FrameworkDatabase::has_class_on_classpath_prefix(self.0, project, prefix)
    }
}

impl<A, DB> DiagnosticProvider<DB> for FrameworkAnalyzerAdapter<A>
where
    A: FrameworkAnalyzer + Send + Sync + 'static,
    DB: ?Sized + Send + Sync + FrameworkDatabase,
{
    fn id(&self) -> &str {
        &self.id
    }

    fn is_applicable(&self, ctx: &ExtensionContext<DB>) -> bool {
        if ctx.cancel.is_cancelled() {
            return false;
        }

        self.cached_applicability(ctx.project).unwrap_or(true)
    }

    fn provide_diagnostics(
        &self,
        ctx: ExtensionContext<DB>,
        params: DiagnosticParams,
    ) -> Vec<Diagnostic> {
        if ctx.cancel.is_cancelled() {
            return Vec::new();
        }
        if !self.ensure_applicability(&ctx) {
            return Vec::new();
        }
        if ctx.cancel.is_cancelled() {
            return Vec::new();
        }

        let db = FrameworkDatabaseView(ctx.db.as_ref());
        self.analyzer
            .diagnostics_with_cancel(&db, params.file, &ctx.cancel)
    }
}

impl<A, DB> CompletionProvider<DB> for FrameworkAnalyzerAdapter<A>
where
    A: FrameworkAnalyzer + Send + Sync + 'static,
    DB: ?Sized + Send + Sync + FrameworkDatabase,
{
    fn id(&self) -> &str {
        &self.id
    }

    fn is_applicable(&self, ctx: &ExtensionContext<DB>) -> bool {
        if ctx.cancel.is_cancelled() {
            return false;
        }

        self.cached_applicability(ctx.project).unwrap_or(true)
    }

    fn provide_completions(
        &self,
        ctx: ExtensionContext<DB>,
        params: CompletionParams,
    ) -> Vec<CompletionItem> {
        if ctx.cancel.is_cancelled() {
            return Vec::new();
        }
        if !self.ensure_applicability(&ctx) {
            return Vec::new();
        }
        if ctx.cancel.is_cancelled() {
            return Vec::new();
        }

        let db = FrameworkDatabaseView(ctx.db.as_ref());
        let completion_ctx = FrameworkCompletionContext {
            project: ctx.project,
            file: params.file,
            offset: params.offset,
        };
        self.analyzer
            .completions_with_cancel(&db, &completion_ctx, &ctx.cancel)
    }
}

impl<A, DB> NavigationProvider<DB> for FrameworkAnalyzerAdapter<A>
where
    A: FrameworkAnalyzer + Send + Sync + 'static,
    DB: ?Sized + Send + Sync + FrameworkDatabase,
{
    fn id(&self) -> &str {
        &self.id
    }

    fn is_applicable(&self, ctx: &ExtensionContext<DB>) -> bool {
        if ctx.cancel.is_cancelled() {
            return false;
        }

        self.cached_applicability(ctx.project).unwrap_or(true)
    }

    fn provide_navigation(
        &self,
        ctx: ExtensionContext<DB>,
        params: NavigationParams,
    ) -> Vec<NavigationTarget> {
        if ctx.cancel.is_cancelled() {
            return Vec::new();
        }
        if !self.ensure_applicability(&ctx) {
            return Vec::new();
        }
        if ctx.cancel.is_cancelled() {
            return Vec::new();
        }

        let db = FrameworkDatabaseView(ctx.db.as_ref());
        let symbol = match params.symbol {
            Symbol::File(file) => FrameworkSymbol::File(file),
            Symbol::Class(class) => FrameworkSymbol::Class(class),
        };

        self.analyzer
            .navigation_with_cancel(&db, &symbol, &ctx.cancel)
            .into_iter()
            .map(|target| NavigationTarget {
                file: target.file,
                span: target.span,
                label: target.label,
            })
            .collect()
    }
}

impl<A, DB> InlayHintProvider<DB> for FrameworkAnalyzerAdapter<A>
where
    A: FrameworkAnalyzer + Send + Sync + 'static,
    DB: ?Sized + Send + Sync + FrameworkDatabase,
{
    fn id(&self) -> &str {
        &self.id
    }

    fn is_applicable(&self, ctx: &ExtensionContext<DB>) -> bool {
        if ctx.cancel.is_cancelled() {
            return false;
        }

        self.cached_applicability(ctx.project).unwrap_or(true)
    }

    fn provide_inlay_hints(
        &self,
        ctx: ExtensionContext<DB>,
        params: InlayHintParams,
    ) -> Vec<InlayHint> {
        if ctx.cancel.is_cancelled() {
            return Vec::new();
        }
        if !self.ensure_applicability(&ctx) {
            return Vec::new();
        }
        if ctx.cancel.is_cancelled() {
            return Vec::new();
        }

        let db = FrameworkDatabaseView(ctx.db.as_ref());
        self.analyzer
            .inlay_hints_with_cancel(&db, params.file, &ctx.cancel)
            .into_iter()
            .map(|hint| InlayHint {
                span: hint.span,
                label: hint.label,
            })
            .collect()
    }
}

impl<A> DiagnosticProvider<dyn nova_db::Database + Send + Sync>
    for FrameworkAnalyzerOnTextDbAdapter<A>
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
        let Some(fw_db) =
            crate::framework_db::framework_db_for_file(ctx.db.clone(), params.file, &ctx.cancel)
        else {
            return Vec::new();
        };

        let project = fw_db.project_of_file(params.file);
        if !self.analyzer.applies_to(fw_db.as_ref(), project) {
            return Vec::new();
        }

        self.analyzer
            .diagnostics_with_cancel(fw_db.as_ref(), params.file, &ctx.cancel)
    }
}

impl<A> CompletionProvider<dyn nova_db::Database + Send + Sync>
    for FrameworkAnalyzerOnTextDbAdapter<A>
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
        let Some(fw_db) =
            crate::framework_db::framework_db_for_file(ctx.db.clone(), params.file, &ctx.cancel)
        else {
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
        self.analyzer
            .completions_with_cancel(fw_db.as_ref(), &completion_ctx, &ctx.cancel)
    }
}

impl<A> NavigationProvider<dyn nova_db::Database + Send + Sync>
    for FrameworkAnalyzerOnTextDbAdapter<A>
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
        let (fw_db, project, symbol) = match params.symbol {
            Symbol::File(file) => {
                let Some(fw_db) =
                    crate::framework_db::framework_db_for_file(ctx.db.clone(), file, &ctx.cancel)
                else {
                    return Vec::new();
                };

                let project = fw_db.project_of_file(file);
                (fw_db, project, FrameworkSymbol::File(file))
            }
            Symbol::Class(class) => {
                // `framework_db_for_file` needs a file to anchor a root-scoped framework DB. For
                // class-based navigation we don't have that information, so we attempt a
                // best-effort fallback: pick any file known to the host DB, build a framework DB
                // from it, then (if possible) re-anchor on a file returned by `all_files`.
                //
                // Limitation: the `ClassId` namespace used by `nova-ext` is not guaranteed to match
                // the best-effort class ids produced by `framework_db`, so class navigation may
                // return empty even when analyzers support it.
                let seed_file = match ctx.db.all_file_ids().into_iter().next() {
                    Some(file) => file,
                    None => return Vec::new(),
                };

                let Some(seed_db) = crate::framework_db::framework_db_for_file(
                    ctx.db.clone(),
                    seed_file,
                    &ctx.cancel,
                ) else {
                    return Vec::new();
                };

                let seed_project = seed_db.project_of_file(seed_file);
                if !self.analyzer.applies_to(seed_db.as_ref(), seed_project) {
                    return Vec::new();
                }

                let anchor_file = seed_db
                    .all_files(seed_project)
                    .into_iter()
                    .next()
                    .unwrap_or(seed_file);

                let Some(fw_db) = crate::framework_db::framework_db_for_file(
                    ctx.db.clone(),
                    anchor_file,
                    &ctx.cancel,
                ) else {
                    return Vec::new();
                };

                let project = fw_db.project_of_file(anchor_file);
                // Avoid panics in analyzers that call `db.class(class)` by checking the class id is
                // present in this root-scoped DB's class set.
                if !fw_db.all_classes(project).contains(&class) {
                    return Vec::new();
                }

                (fw_db, project, FrameworkSymbol::Class(class))
            }
        };

        if !self.analyzer.applies_to(fw_db.as_ref(), project) {
            return Vec::new();
        }

        self.analyzer
            .navigation_with_cancel(fw_db.as_ref(), &symbol, &ctx.cancel)
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
    for FrameworkAnalyzerOnTextDbAdapter<A>
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
        let Some(fw_db) =
            crate::framework_db::framework_db_for_file(ctx.db.clone(), params.file, &ctx.cancel)
        else {
            return Vec::new();
        };

        let project = fw_db.project_of_file(params.file);
        if !self.analyzer.applies_to(fw_db.as_ref(), project) {
            return Vec::new();
        }

        self.analyzer
            .inlay_hints_with_cancel(fw_db.as_ref(), params.file, &ctx.cancel)
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
    build_metadata_only: bool,
}

impl FrameworkAnalyzerRegistryProvider {
    pub fn new(registry: Arc<AnalyzerRegistry>) -> Self {
        Self {
            registry,
            fast_noop: false,
            build_metadata_only: false,
        }
    }

    /// Restrict this provider to projects that have authoritative build metadata (Maven/Gradle/Bazel).
    ///
    /// When enabled, the provider returns empty results for "simple" projects (directories without
    /// a supported build system). This is useful when running the analyzer registry alongside
    /// Nova's legacy `framework_cache` providers to avoid duplicate diagnostics/completions.
    pub fn with_build_metadata_only(mut self) -> Self {
        self.build_metadata_only = true;
        self
    }

    /// Construct a provider that always returns empty results without attempting to build the
    /// framework database.
    ///
    /// This can be useful when a consumer wants to reserve the provider ID in an
    /// [`ExtensionRegistry`] without paying per-request overhead, while still allowing the provider
    /// to be replaced with a real analyzer registry later.
    pub fn empty() -> Self {
        Self {
            registry: Arc::new(AnalyzerRegistry::new()),
            fast_noop: true,
            build_metadata_only: false,
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

impl<DB: ?Sized> DiagnosticProvider<DB> for FrameworkAnalyzerRegistryProvider
where
    DB: Send + Sync + 'static + nova_db::Database + AsDynNovaDb,
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
        let host_db = ctx.db.clone().into_dyn_nova_db();
        if self.build_metadata_only && !has_build_metadata(host_db.as_ref(), params.file) {
            return Vec::new();
        }
        let Some(fw_db) = self.framework_db(host_db, params.file, &ctx.cancel) else {
            return Vec::new();
        };
        self.registry
            .framework_diagnostics_with_cancel(fw_db.as_ref(), params.file, &ctx.cancel)
    }
}

impl<DB: ?Sized> CompletionProvider<DB> for FrameworkAnalyzerRegistryProvider
where
    DB: Send + Sync + 'static + nova_db::Database + AsDynNovaDb,
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
        let host_db = ctx.db.clone().into_dyn_nova_db();
        if self.build_metadata_only && !has_build_metadata(host_db.as_ref(), params.file) {
            return Vec::new();
        }
        let Some(fw_db) = self.framework_db(host_db, params.file, &ctx.cancel) else {
            return Vec::new();
        };

        let project = fw_db.project_of_file(params.file);
        let completion_ctx = FrameworkCompletionContext {
            project,
            file: params.file,
            offset: params.offset,
        };
        self.registry.framework_completions_with_cancel(
            fw_db.as_ref(),
            &completion_ctx,
            &ctx.cancel,
        )
    }
}

impl<DB: ?Sized> NavigationProvider<DB> for FrameworkAnalyzerRegistryProvider
where
    DB: Send + Sync + 'static + nova_db::Database + AsDynNovaDb,
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

        let host_db = ctx.db.clone().into_dyn_nova_db();
        if self.build_metadata_only && !has_build_metadata(host_db.as_ref(), file) {
            return Vec::new();
        }
        let Some(fw_db) = self.framework_db(host_db, file, &ctx.cancel) else {
            return Vec::new();
        };

        let symbol = match params.symbol {
            Symbol::File(file) => FrameworkSymbol::File(file),
            Symbol::Class(class) => FrameworkSymbol::Class(class),
        };

        self.registry
            .framework_navigation_targets_with_cancel(fw_db.as_ref(), &symbol, &ctx.cancel)
            .into_iter()
            .map(|target| NavigationTarget {
                file: target.file,
                span: target.span,
                label: target.label,
            })
            .collect()
    }
}

impl<DB: ?Sized> InlayHintProvider<DB> for FrameworkAnalyzerRegistryProvider
where
    DB: Send + Sync + 'static + nova_db::Database + AsDynNovaDb,
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
        let host_db = ctx.db.clone().into_dyn_nova_db();
        if self.build_metadata_only && !has_build_metadata(host_db.as_ref(), params.file) {
            return Vec::new();
        }
        let Some(fw_db) = self.framework_db(host_db, params.file, &ctx.cancel) else {
            return Vec::new();
        };

        self.registry
            .framework_inlay_hints_with_cancel(fw_db.as_ref(), params.file, &ctx.cancel)
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
    FrameworkAnalyzerRegistryProvider: DiagnosticProvider<DB> + CompletionProvider<DB>,
{
    pub fn with_default_registry(db: Arc<DB>, config: Arc<NovaConfig>, project: ProjectId) -> Self {
        let mut this = Self::new(db, config, project);
        let registry = this.registry_mut();
        let _ = registry.register_diagnostic_provider(Arc::new(FrameworkDiagnosticProvider));
        let _ = registry.register_completion_provider(Arc::new(FrameworkCompletionProvider));

        let fw_registry = nova_framework_builtins::builtin_registry();
        let provider = FrameworkAnalyzerRegistryProvider::new(Arc::new(fw_registry))
            .with_build_metadata_only()
            .into_arc();
        let _ = registry.register_diagnostic_provider(provider.clone());
        let _ = registry.register_completion_provider(provider.clone());
        let _ = registry.register_navigation_provider(provider.clone());
        let _ = registry.register_inlay_hint_provider(provider);
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
        if cancel.is_cancelled() {
            return Vec::new();
        }
        fn severity_rank(severity: nova_ext::Severity) -> u8 {
            match severity {
                nova_ext::Severity::Error => 0,
                nova_ext::Severity::Warning => 1,
                nova_ext::Severity::Info => 2,
            }
        }

        fn span_key(span: Option<Span>) -> (usize, usize) {
            match span {
                Some(span) => (span.start, span.end),
                None => (usize::MAX, usize::MAX),
            }
        }

        let mut diagnostics = crate::code_intelligence::core_file_diagnostics_cancelable(
            self.db.as_ref().as_dyn_nova_db(),
            file,
            &cancel,
        );
        if cancel.is_cancelled() {
            return Vec::new();
        }
        // Keep built-in diagnostics ordering consistent between:
        // - `nova_lsp::diagnostics` (which goes through `nova_ide::file_diagnostics_lsp`)
        // - `nova_lsp::diagnostics_with_extensions` (which goes through this helper).
        //
        // `core_file_diagnostics_cancelable` intentionally emits diagnostics in discovery order,
        // but the public diagnostics surface sorts/dedupes them for determinism. Mirror that here
        // so adding extensions does not reorder built-in diagnostics.
        diagnostics.sort_by(|a, b| {
            span_key(a.span)
                .cmp(&span_key(b.span))
                .then_with(|| severity_rank(a.severity).cmp(&severity_rank(b.severity)))
                .then_with(|| a.code.as_ref().cmp(b.code.as_ref()))
                .then_with(|| a.message.cmp(&b.message))
        });
        diagnostics.dedup();
        if cancel.is_cancelled() {
            return Vec::new();
        }
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
            &cancel,
        );
        let text = self.db.file_content(file);
        let text_index = TextIndex::new(text);
        let offset = text_index
            .position_to_offset(position)
            .unwrap_or(text.len());

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
        if cancel.is_cancelled() {
            return Vec::new();
        }

        let mut actions = Vec::new();

        let source = self.db.file_content(file);
        let source_index = TextIndex::new(source);
        let uri: Option<lsp_types::Uri> = self
            .db
            .file_path(file)
            .and_then(|path| nova_core::AbsPathBuf::new(path.to_path_buf()).ok())
            .and_then(|path| nova_core::path_to_file_uri(&path).ok())
            .and_then(|uri| uri.parse().ok());

        if let Some(uri) = uri.clone() {
            if source.contains("import") {
                let refactor_file = RefactorFileId::new(uri.to_string());
                let db = TextDatabase::new([(refactor_file.clone(), source.to_string())]);
                if let Ok(edit) = organize_imports(
                    &db,
                    OrganizeImportsParams {
                        file: refactor_file.clone(),
                    },
                ) {
                    if !edit.is_empty() {
                        if let Ok(lsp_edit) = workspace_edit_to_lsp(&db, &edit) {
                            actions.push(lsp_types::CodeActionOrCommand::CodeAction(
                                lsp_types::CodeAction {
                                    title: "Organize imports".to_string(),
                                    kind: Some(lsp_types::CodeActionKind::SOURCE_ORGANIZE_IMPORTS),
                                    edit: Some(lsp_edit),
                                    is_preferred: Some(true),
                                    ..lsp_types::CodeAction::default()
                                },
                            ));
                        }
                    }
                }
            }
        }

        if let (Some(uri), Some(span)) = (uri, span) {
            let selection = source_index.span_to_lsp_range(span);

            // Quick-fix code actions are latency-sensitive. Avoid running full/workspace-scoped
            // diagnostics here; compute only the minimal diagnostics needed for quick fixes.
            let diagnostics = crate::code_intelligence::diagnostics_for_quick_fixes(
                self.db.as_ref().as_dyn_nova_db(),
                file,
                &cancel,
            );
            actions.extend(crate::quick_fix::quick_fixes_for_diagnostics(
                &uri,
                source,
                span,
                &diagnostics,
            ));
            actions.extend(
                crate::quick_fixes::create_symbol_quick_fixes_from_diagnostics(
                    &uri,
                    source,
                    Some(span),
                    &diagnostics,
                ),
            );

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
            actions.extend(type_mismatch_quick_fixes(
                &cancel,
                source,
                &uri,
                span,
                &diagnostics,
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

        dedupe_code_actions_by_kind_and_title(&mut actions);

        actions
    }

    pub fn code_actions_lsp_with_context(
        &self,
        cancel: CancellationToken,
        file: nova_ext::FileId,
        span: Option<Span>,
        context_diagnostics: &[lsp_types::Diagnostic],
    ) -> Vec<lsp_types::CodeActionOrCommand> {
        let mut actions = Vec::new();

        let source = self.db.file_content(file);
        let source_index = TextIndex::new(source);
        let uri: Option<lsp_types::Uri> = self
            .db
            .file_path(file)
            .and_then(|path| nova_core::AbsPathBuf::new(path.to_path_buf()).ok())
            .and_then(|path| nova_core::path_to_file_uri(&path).ok())
            .and_then(|uri| uri.parse().ok());

        // These source-level refactors do not depend on diagnostics context.
        if let Some(uri) = uri.clone() {
            if source.contains("import") {
                let refactor_file = RefactorFileId::new(uri.to_string());
                let db = TextDatabase::new([(refactor_file.clone(), source.to_string())]);
                if let Ok(edit) = organize_imports(
                    &db,
                    OrganizeImportsParams {
                        file: refactor_file.clone(),
                    },
                ) {
                    if !edit.is_empty() {
                        if let Ok(lsp_edit) = workspace_edit_to_lsp(&db, &edit) {
                            actions.push(lsp_types::CodeActionOrCommand::CodeAction(
                                lsp_types::CodeAction {
                                    title: "Organize imports".to_string(),
                                    kind: Some(lsp_types::CodeActionKind::SOURCE_ORGANIZE_IMPORTS),
                                    edit: Some(lsp_edit),
                                    is_preferred: Some(true),
                                    ..lsp_types::CodeAction::default()
                                },
                            ));
                        }
                    }
                }
            }
        }

        if cancel.is_cancelled() {
            return actions;
        }

        if let (Some(uri), Some(span)) = (uri, span) {
            let selection = source_index.span_to_lsp_range(span);

            // Add diagnostic-driven quick fixes (parity with stdio server) using only the
            // diagnostics provided by the LSP client (`CodeActionContext.diagnostics`).
            let diag_actions = crate::code_action::diagnostic_quick_fixes(
                source,
                Some(uri.clone()),
                selection.clone(),
                context_diagnostics,
            );
            actions.extend(
                diag_actions
                    .into_iter()
                    .map(lsp_types::CodeActionOrCommand::CodeAction),
            );

            // Convert LSP diagnostics to Nova diagnostics so we can reuse existing quick-fix logic
            // without recomputing diagnostics for the whole file.
            let mut diagnostics = Vec::new();
            for diagnostic in context_diagnostics {
                let Some(lsp_types::NumberOrString::String(code)) = diagnostic.code.as_ref() else {
                    continue;
                };
                let Some(start) = source_index.position_to_offset(diagnostic.range.start) else {
                    continue;
                };
                let Some(end) = source_index.position_to_offset(diagnostic.range.end) else {
                    continue;
                };
                let severity = match diagnostic.severity {
                    Some(lsp_types::DiagnosticSeverity::ERROR) => nova_ext::Severity::Error,
                    Some(lsp_types::DiagnosticSeverity::WARNING) => nova_ext::Severity::Warning,
                    Some(lsp_types::DiagnosticSeverity::INFORMATION)
                    | Some(lsp_types::DiagnosticSeverity::HINT)
                    | None => nova_ext::Severity::Info,
                    // Be forward-compatible with unknown severities.
                    Some(_) => nova_ext::Severity::Info,
                };
                diagnostics.push(Diagnostic {
                    severity,
                    code: Cow::Owned(code.clone()),
                    message: diagnostic.message.clone(),
                    span: Some(Span::new(start, end)),
                });
            }
            actions.extend(crate::quick_fix::quick_fixes_for_diagnostics(
                &uri,
                source,
                span,
                &diagnostics,
            ));
            actions.extend(crate::quick_fixes::create_symbol_quick_fixes(
                self.db.as_ref().as_dyn_nova_db(),
                file,
                Some(span),
            ));

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
            // Quick fixes driven by diagnostics should prefer the diagnostics passed by the LSP
            // client (`CodeActionContext.diagnostics`) so we don't need to recompute diagnostics
            // for the whole file.
            actions.extend(return_mismatch_quick_fixes_from_context(
                &cancel,
                source,
                &uri,
                span,
                context_diagnostics,
            ));
            actions.extend(type_mismatch_quick_fixes_from_context(
                &cancel,
                source,
                &uri,
                span,
                context_diagnostics,
            ));
            actions.extend(unused_import_quick_fixes_from_context(
                &cancel,
                source,
                &uri,
                span,
                context_diagnostics,
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

        dedupe_code_actions_by_kind_and_title(&mut actions);

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
        let end_offset = text_index
            .position_to_offset(range.end)
            .unwrap_or(text.len());

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

fn dedupe_code_actions_by_kind_and_title(actions: &mut Vec<lsp_types::CodeActionOrCommand>) {
    // Some quick-fix sources overlap (e.g. multiple passes offering the same "Create field"
    // action). Dedupe by (kind, title) to avoid noisy duplicate entries in the UI.
    //
    // When duplicates occur, keep the more preferred variant (i.e. one with `is_preferred=true`)
    // since LSP clients often surface preferred actions more prominently.
    let existing = std::mem::take(actions);
    let mut out: Vec<lsp_types::CodeActionOrCommand> = Vec::with_capacity(existing.len());
    let mut seen: HashMap<(Option<lsp_types::CodeActionKind>, String), usize> = HashMap::new();

    fn is_preferred(action: &lsp_types::CodeActionOrCommand) -> bool {
        match action {
            lsp_types::CodeActionOrCommand::CodeAction(action) => {
                action.is_preferred.unwrap_or(false)
            }
            lsp_types::CodeActionOrCommand::Command(_) => false,
        }
    }

    for action in existing {
        let (kind, title) = match &action {
            lsp_types::CodeActionOrCommand::CodeAction(action) => {
                (action.kind.clone(), action.title.clone())
            }
            lsp_types::CodeActionOrCommand::Command(command) => (None, command.title.clone()),
        };

        let key = (kind, title);
        if let Some(&idx) = seen.get(&key) {
            if is_preferred(&action) && !is_preferred(&out[idx]) {
                out[idx] = action;
            }
            continue;
        }

        seen.insert(key, out.len());
        out.push(action);
    }

    *actions = out;
}

fn type_mismatch_quick_fixes(
    cancel: &CancellationToken,
    source: &str,
    uri: &lsp_types::Uri,
    selection: Span,
    diagnostics: &[Diagnostic],
) -> Vec<lsp_types::CodeActionOrCommand> {
    fn cast_replacement(expected: &str, expr: &str) -> String {
        // Java casts apply to a *unary* expression. Without parentheses, `({T}) a + b` parses as
        // `((T) a) + b` and does not cast the whole expression.
        let needs_parens = expr.chars().any(|c| c.is_whitespace())
            || [
                "+", "-", "*", "/", "%", "?", ":", "&&", "||", "==", "!=", "<", ">", "=", "&", "|",
                "^",
            ]
            .into_iter()
            .any(|op| expr.contains(op));

        if needs_parens {
            format!("({expected}) ({expr})")
        } else {
            format!("({expected}) {expr}")
        }
    }
    fn parse_type_mismatch(message: &str) -> Option<(String, String)> {
        let message = message.strip_prefix("type mismatch: expected ")?;
        let (expected, found) = message.split_once(", found ")?;
        Some((expected.trim().to_string(), found.trim().to_string()))
    }

    fn single_replace_edit(
        uri: &lsp_types::Uri,
        range: lsp_types::Range,
        new_text: String,
    ) -> lsp_types::WorkspaceEdit {
        let mut changes: HashMap<lsp_types::Uri, Vec<lsp_types::TextEdit>> = HashMap::new();
        changes.insert(uri.clone(), vec![lsp_types::TextEdit { range, new_text }]);
        lsp_types::WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        }
    }

    if cancel.is_cancelled() {
        return Vec::new();
    }

    let mut actions = Vec::new();
    let source_index = TextIndex::new(source);
    for diag in diagnostics {
        if cancel.is_cancelled() {
            return Vec::new();
        }
        if diag.code.as_ref() != "type-mismatch" {
            continue;
        }
        let Some(diag_span) = diag.span else {
            continue;
        };
        if !crate::quick_fix::spans_intersect(selection, diag_span) {
            continue;
        }

        let Some((expected, _found)) = parse_type_mismatch(&diag.message) else {
            continue;
        };

        let expr = source
            .get(diag_span.start..diag_span.end)
            .unwrap_or_default()
            .trim();
        if expr.is_empty() {
            continue;
        }

        let range = source_index.span_to_lsp_range(diag_span);

        if expected == "String" {
            let edit = single_replace_edit(uri, range.clone(), format!("String.valueOf({expr})"));
            actions.push(lsp_types::CodeActionOrCommand::CodeAction(
                lsp_types::CodeAction {
                    title: "Convert to String".to_string(),
                    kind: Some(lsp_types::CodeActionKind::QUICKFIX),
                    edit: Some(edit),
                    is_preferred: Some(true),
                    ..lsp_types::CodeAction::default()
                },
            ));
        }

        let edit = single_replace_edit(uri, range, cast_replacement(&expected, expr));
        actions.push(lsp_types::CodeActionOrCommand::CodeAction(
            lsp_types::CodeAction {
                title: format!("Cast to {expected}"),
                kind: Some(lsp_types::CodeActionKind::QUICKFIX),
                edit: Some(edit),
                is_preferred: Some(expected != "String"),
                ..lsp_types::CodeAction::default()
            },
        ));
    }

    actions
}

fn type_mismatch_quick_fixes_from_context(
    cancel: &CancellationToken,
    source: &str,
    uri: &lsp_types::Uri,
    selection: Span,
    context_diagnostics: &[lsp_types::Diagnostic],
) -> Vec<lsp_types::CodeActionOrCommand> {
    fn cast_replacement(expected: &str, expr: &str) -> String {
        // Java casts apply to a *unary* expression. Without parentheses, `({T}) a + b` parses as
        // `((T) a) + b` and does not cast the whole expression.
        let needs_parens = expr.chars().any(|c| c.is_whitespace())
            || [
                "+", "-", "*", "/", "%", "?", ":", "&&", "||", "==", "!=", "<", ">", "=", "&", "|",
                "^",
            ]
            .into_iter()
            .any(|op| expr.contains(op));

        if needs_parens {
            format!("({expected}) ({expr})")
        } else {
            format!("({expected}) {expr}")
        }
    }
    fn parse_type_mismatch(message: &str) -> Option<(String, String)> {
        let message = message.strip_prefix("type mismatch: expected ")?;
        let (expected, found) = message.split_once(", found ")?;
        Some((expected.trim().to_string(), found.trim().to_string()))
    }

    fn single_replace_edit(
        uri: &lsp_types::Uri,
        range: lsp_types::Range,
        new_text: String,
    ) -> lsp_types::WorkspaceEdit {
        let mut changes: HashMap<lsp_types::Uri, Vec<lsp_types::TextEdit>> = HashMap::new();
        changes.insert(uri.clone(), vec![lsp_types::TextEdit { range, new_text }]);
        lsp_types::WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        }
    }

    if cancel.is_cancelled() {
        return Vec::new();
    }
    let source_index = TextIndex::new(source);
    let mut actions = Vec::new();
    for diagnostic in context_diagnostics {
        if cancel.is_cancelled() {
            return Vec::new();
        }
        let Some(lsp_types::NumberOrString::String(code)) = diagnostic.code.as_ref() else {
            continue;
        };
        if code != "type-mismatch" {
            continue;
        }

        let Some(start) = source_index.position_to_offset(diagnostic.range.start) else {
            continue;
        };
        let Some(end) = source_index.position_to_offset(diagnostic.range.end) else {
            continue;
        };
        let diag_span = Span::new(start, end);
        if !crate::quick_fix::spans_intersect(selection, diag_span) {
            continue;
        }

        let Some((expected, _found)) = parse_type_mismatch(&diagnostic.message) else {
            continue;
        };

        let expr = source
            .get(diag_span.start..diag_span.end)
            .unwrap_or_default()
            .trim();
        if expr.is_empty() {
            continue;
        }

        let range = diagnostic.range.clone();

        if expected == "String" {
            let edit = single_replace_edit(uri, range.clone(), format!("String.valueOf({expr})"));
            actions.push(lsp_types::CodeActionOrCommand::CodeAction(
                lsp_types::CodeAction {
                    title: "Convert to String".to_string(),
                    kind: Some(lsp_types::CodeActionKind::QUICKFIX),
                    edit: Some(edit),
                    is_preferred: Some(true),
                    ..lsp_types::CodeAction::default()
                },
            ));
        }

        let edit = single_replace_edit(uri, range, cast_replacement(&expected, expr));
        actions.push(lsp_types::CodeActionOrCommand::CodeAction(
            lsp_types::CodeAction {
                title: format!("Cast to {expected}"),
                kind: Some(lsp_types::CodeActionKind::QUICKFIX),
                edit: Some(edit),
                is_preferred: Some(expected != "String"),
                ..lsp_types::CodeAction::default()
            },
        ));
    }

    actions
}

fn return_mismatch_quick_fixes_from_context(
    cancel: &CancellationToken,
    source: &str,
    uri: &lsp_types::Uri,
    selection: Span,
    context_diagnostics: &[lsp_types::Diagnostic],
) -> Vec<lsp_types::CodeActionOrCommand> {
    fn parse_return_mismatch(message: &str) -> Option<(String, String)> {
        // Current format (from Salsa typeck):
        // `return type mismatch: expected {expected}, found {found}`
        let message = message.strip_prefix("return type mismatch: expected ")?;
        let (expected, found) = message.split_once(", found ")?;
        Some((expected.trim().to_string(), found.trim().to_string()))
    }

    fn single_replace_edit(
        uri: &lsp_types::Uri,
        range: lsp_types::Range,
        new_text: String,
    ) -> lsp_types::WorkspaceEdit {
        let mut changes: HashMap<lsp_types::Uri, Vec<lsp_types::TextEdit>> = HashMap::new();
        changes.insert(uri.clone(), vec![lsp_types::TextEdit { range, new_text }]);
        lsp_types::WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        }
    }

    if cancel.is_cancelled() {
        return Vec::new();
    }

    let mut actions = Vec::new();

    let source_index = TextIndex::new(source);
    for diagnostic in context_diagnostics {
        if cancel.is_cancelled() {
            return Vec::new();
        }
        let Some(lsp_types::NumberOrString::String(code)) = diagnostic.code.as_ref() else {
            continue;
        };
        if code != "return-mismatch" {
            continue;
        }

        let Some(start) = source_index.position_to_offset(diagnostic.range.start) else {
            continue;
        };
        let Some(end) = source_index.position_to_offset(diagnostic.range.end) else {
            continue;
        };
        let diag_span = Span::new(start, end);
        if !crate::quick_fix::spans_intersect(selection, diag_span) {
            continue;
        }

        if diagnostic
            .message
            .contains("cannot return a value from a `void` method")
        {
            let edit = single_replace_edit(uri, diagnostic.range.clone(), String::new());
            actions.push(lsp_types::CodeActionOrCommand::CodeAction(
                lsp_types::CodeAction {
                    title: "Remove returned value".to_string(),
                    kind: Some(lsp_types::CodeActionKind::QUICKFIX),
                    edit: Some(edit),
                    ..lsp_types::CodeAction::default()
                },
            ));
            continue;
        }

        let Some((expected, found)) = parse_return_mismatch(&diagnostic.message) else {
            continue;
        };
        if found == "void" {
            continue;
        }

        let expr = source
            .get(diag_span.start..diag_span.end)
            .unwrap_or_default()
            .trim();
        if expr.is_empty() {
            continue;
        }

        let replacement = format!("({expected}) ({expr})");
        let edit = single_replace_edit(uri, diagnostic.range.clone(), replacement);
        actions.push(lsp_types::CodeActionOrCommand::CodeAction(
            lsp_types::CodeAction {
                title: format!("Cast to {expected}"),
                kind: Some(lsp_types::CodeActionKind::QUICKFIX),
                edit: Some(edit),
                ..lsp_types::CodeAction::default()
            },
        ));
    }

    actions
}
fn unused_import_quick_fixes_from_context(
    cancel: &CancellationToken,
    source: &str,
    uri: &lsp_types::Uri,
    selection: Span,
    context_diagnostics: &[lsp_types::Diagnostic],
) -> Vec<lsp_types::CodeActionOrCommand> {
    fn single_delete_edit(
        uri: &lsp_types::Uri,
        range: lsp_types::Range,
    ) -> lsp_types::WorkspaceEdit {
        let mut changes: HashMap<lsp_types::Uri, Vec<lsp_types::TextEdit>> = HashMap::new();
        changes.insert(
            uri.clone(),
            vec![lsp_types::TextEdit {
                range,
                new_text: String::new(),
            }],
        );
        lsp_types::WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        }
    }

    if cancel.is_cancelled() {
        return Vec::new();
    }

    let mut actions = Vec::new();

    let source_index = TextIndex::new(source);
    for diagnostic in context_diagnostics {
        if cancel.is_cancelled() {
            return Vec::new();
        }
        let Some(lsp_types::NumberOrString::String(code)) = diagnostic.code.as_ref() else {
            continue;
        };
        if code != "unused-import" {
            continue;
        }

        let Some(start) = source_index.position_to_offset(diagnostic.range.start) else {
            continue;
        };
        let Some(end) = source_index.position_to_offset(diagnostic.range.end) else {
            continue;
        };
        let diag_span = Span::new(start, end);
        if !crate::quick_fix::spans_intersect(selection, diag_span) {
            continue;
        }

        let line_start = source
            .get(..diag_span.start)
            .unwrap_or("")
            .rfind('\n')
            .map(|idx| idx + 1)
            .unwrap_or(0);

        let line_end = source
            .get(diag_span.end..)
            .unwrap_or("")
            .find('\n')
            .map(|idx| diag_span.end + idx + 1)
            .unwrap_or(source.len());

        let range = lsp_types::Range::new(
            source_index.offset_to_position(line_start),
            source_index.offset_to_position(line_end),
        );
        let edit = single_delete_edit(uri, range);
        actions.push(lsp_types::CodeActionOrCommand::CodeAction(
            lsp_types::CodeAction {
                title: "Remove unused import".to_string(),
                kind: Some(lsp_types::CodeActionKind::QUICKFIX),
                edit: Some(edit),
                is_preferred: Some(true),
                ..lsp_types::CodeAction::default()
            },
        ));
    }

    actions
}

struct FrameworkDiagnosticProvider;

fn has_build_metadata(db: &dyn nova_db::Database, file: nova_ext::FileId) -> bool {
    let Some(path) = db.file_path(file) else {
        return false;
    };
    let root = crate::framework_cache::project_root_for_path(path);
    let Some(config) = crate::framework_cache::project_config(&root) else {
        return false;
    };
    // `nova_project::ProjectConfig` can also be synthesized for "simple" projects (a directory with
    // a `src/` folder). Treat "real" build systems (Maven/Gradle/Bazel) as authoritative build
    // metadata for analyzer-based framework intelligence.
    config.build_system != nova_project::BuildSystem::Simple
}

fn mapstruct_diagnostics_when_build_metadata_reports_missing_dependency(
    db: &dyn nova_db::Database,
    file: nova_ext::FileId,
    cancel: &CancellationToken,
) -> Vec<Diagnostic> {
    if cancel.is_cancelled() {
        return Vec::new();
    }
    let Some(path) = db.file_path(file) else {
        return Vec::new();
    };
    let text = db.file_content(file);
    let maybe_mapstruct_file = text.contains("org.mapstruct");
    if !maybe_mapstruct_file {
        return Vec::new();
    }

    // Only attempt MapStruct missing dependency diagnostics when we have build metadata and it
    // definitively indicates MapStruct isn't present. For "Simple" projects (no build metadata),
    // suppress noisy false positives and rely on the legacy framework cache behavior.
    let root = crate::framework_cache::project_root_for_path(path);
    let Some(config) = crate::framework_cache::project_config(&root) else {
        return Vec::new();
    };
    if config.build_system == nova_project::BuildSystem::Simple {
        return Vec::new();
    }

    let has_mapstruct_dependency = config.dependencies.iter().any(|dep| {
        dep.group_id == "org.mapstruct"
            && matches!(
                dep.artifact_id.as_str(),
                "mapstruct" | "mapstruct-processor"
            )
    });
    if has_mapstruct_dependency {
        return Vec::new();
    }

    if cancel.is_cancelled() {
        return Vec::new();
    }

    match nova_framework_mapstruct::diagnostics_for_file(
        &root,
        path,
        text,
        has_mapstruct_dependency,
    ) {
        Ok(diags) => diags,
        Err(_) => Vec::new(),
    }
}

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
        if ctx.cancel.is_cancelled() {
            return Vec::new();
        }

        let db = ctx.db.as_ref().as_dyn_nova_db();
        if has_build_metadata(db, params.file) {
            // In "real" build system projects, most framework intelligence should be sourced from
            // analyzer-based providers. However, keep MapStruct missing dependency diagnostics
            // available even when MapStruct is not on the classpath (and therefore would not be
            // considered applicable by the analyzer registry).
            return mapstruct_diagnostics_when_build_metadata_reports_missing_dependency(
                db,
                params.file,
                &ctx.cancel,
            );
        }
        crate::framework_cache::framework_diagnostics(db, params.file, &ctx.cancel)
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
        if ctx.cancel.is_cancelled() {
            return Vec::new();
        }

        let db = ctx.db.as_ref().as_dyn_nova_db();
        if has_build_metadata(db, params.file) {
            return Vec::new();
        }
        crate::framework_cache::framework_completions(db, params.file, params.offset, &ctx.cancel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nova_ext::{CodeActionProvider, CompletionProvider, DiagnosticProvider, InlayHintProvider};
    use nova_framework::{Database, FrameworkAnalyzer, MemoryDatabase};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

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

    struct PanickingAppliesToAnalyzer;

    impl FrameworkAnalyzer for PanickingAppliesToAnalyzer {
        fn applies_to(&self, _db: &dyn Database, _project: ProjectId) -> bool {
            panic!("applies_to panic");
        }

        fn diagnostics(&self, _db: &dyn Database, _file: nova_ext::FileId) -> Vec<Diagnostic> {
            vec![Diagnostic::warning(
                "FW",
                "should not run",
                Some(Span::new(0, 1)),
            )]
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

        let analyzer =
            FrameworkAnalyzerAdapter::new("framework.cancel", CancellationAwareAnalyzer).into_arc();
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
    fn framework_analyzer_adapter_supports_concrete_database_types() {
        let mut db = MemoryDatabase::new();
        let project = db.add_project();
        let file = db.add_file(project);

        let db = Arc::new(db);
        let mut ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), project);

        let analyzer =
            FrameworkAnalyzerAdapter::new("framework.test", FrameworkTestAnalyzer).into_arc();
        ide.registry_mut()
            .register_diagnostic_provider(analyzer.clone())
            .unwrap();
        ide.registry_mut()
            .register_completion_provider(analyzer.clone())
            .unwrap();

        let diags = ide.diagnostics(CancellationToken::new(), file);
        assert_eq!(diags.len(), 1);
        assert!(diags.iter().any(|d| d.message == "framework"));

        let completions = ide.completions(CancellationToken::new(), file, 0);
        assert_eq!(completions.len(), 1);
        assert!(completions.iter().any(|c| c.label == "frameworkCompletion"));
    }

    #[test]
    fn framework_analyzer_adapter_allows_cooperative_cancellation_during_execution() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::mpsc;
        use std::time::Duration;

        struct CooperativeCancelAnalyzer {
            started: mpsc::Sender<()>,
            finished: mpsc::Sender<()>,
            saw_cancel: Arc<AtomicBool>,
        }

        impl FrameworkAnalyzer for CooperativeCancelAnalyzer {
            fn applies_to(&self, _db: &dyn Database, _project: ProjectId) -> bool {
                true
            }

            fn diagnostics_with_cancel(
                &self,
                _db: &dyn Database,
                _file: nova_ext::FileId,
                cancel: &CancellationToken,
            ) -> Vec<Diagnostic> {
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
                vec![Diagnostic::warning(
                    "FW",
                    "framework",
                    Some(Span::new(0, 1)),
                )]
            }
        }

        let mut db = MemoryDatabase::new();
        let project = db.add_project();
        let file = db.add_file(project);

        let db: Arc<dyn Database + Send + Sync> = Arc::new(db);
        let mut ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), project);
        ide.registry_mut().options_mut().diagnostic_timeout = Duration::from_secs(1);

        let (started_tx, started_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let saw_cancel = Arc::new(AtomicBool::new(false));

        let analyzer = FrameworkAnalyzerAdapter::new(
            "framework.coop_cancel",
            CooperativeCancelAnalyzer {
                started: started_tx,
                finished: finished_tx,
                saw_cancel: Arc::clone(&saw_cancel),
            },
        )
        .into_arc();
        ide.registry_mut()
            .register_diagnostic_provider(analyzer)
            .unwrap();

        let cancel = CancellationToken::new();
        let cancel_for_thread = cancel.clone();

        let cancel_thread = std::thread::spawn(move || {
            started_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("analyzer should start");
            cancel_for_thread.cancel();
        });

        let diags = ide.diagnostics(cancel, file);
        assert!(
            diags.is_empty(),
            "expected diagnostics to be empty after cancellation; got {diags:?}"
        );

        cancel_thread.join().unwrap();
        finished_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("analyzer should finish after cancellation");
        assert!(
            saw_cancel.load(Ordering::SeqCst),
            "expected analyzer to observe cancellation"
        );
    }

    #[test]
    fn panicking_framework_analyzer_is_applicable_is_ignored() {
        let mut db = MemoryDatabase::new();
        let project = db.add_project();
        let file = db.add_file(project);

        let db: Arc<dyn Database + Send + Sync> = Arc::new(db);
        let mut ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), project);

        let analyzer =
            FrameworkAnalyzerAdapter::new("framework.panics", PanickingAppliesToAnalyzer)
                .into_arc();
        ide.registry_mut()
            .register_diagnostic_provider(analyzer)
            .unwrap();

        let diags = ide.diagnostics(CancellationToken::new(), file);
        assert!(
            diags.is_empty(),
            "expected no diagnostics because is_applicable panicked; got {diags:?}"
        );
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
    fn all_diagnostics_skips_extension_providers_when_cancelled() {
        use nova_db::InMemoryFileStore;

        struct CountingProvider {
            calls: Arc<AtomicUsize>,
        }

        impl DiagnosticProvider<dyn nova_db::Database + Send + Sync> for CountingProvider {
            fn id(&self) -> &str {
                "counting.diag"
            }

            fn provide_diagnostics(
                &self,
                _ctx: ExtensionContext<dyn nova_db::Database + Send + Sync>,
                _params: DiagnosticParams,
            ) -> Vec<Diagnostic> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Vec::new()
            }
        }

        let mut db = InMemoryFileStore::new();
        let file = db.file_id_for_path(PathBuf::from("/cancelled.java"));
        db.set_file_text(
            file,
            r#"
class A {
  void m() {
    baz();
  }
}
"#
            .to_string(),
        );

        let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(db);
        let mut ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), ProjectId::new(0));

        let calls = Arc::new(AtomicUsize::new(0));
        ide.registry_mut()
            .register_diagnostic_provider(Arc::new(CountingProvider {
                calls: Arc::clone(&calls),
            }))
            .unwrap();

        let cancel = CancellationToken::new();
        cancel.cancel();

        let diags = ide.all_diagnostics(cancel, file);
        assert!(
            diags.is_empty(),
            "expected diagnostics to be empty after cancellation; got {diags:?}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "expected cancelled all_diagnostics request to skip extension diagnostics"
        );
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
    fn offers_quick_fix_code_action_for_type_mismatch() {
        use nova_db::InMemoryFileStore;

        let mut db = InMemoryFileStore::new();
        let file = db.file_id_for_path(PathBuf::from("/quickfix.java"));
        let source = r#"
class A {
  void m() {
    int x = "";
  }
}
"#;
        db.set_file_text(file, source.to_string());

        // Place the cursor inside the string literal so the request span intersects the
        // `type-mismatch` diagnostic span (which is reported on the initializer expression).
        let cursor = source.find("\"\"").expect("string literal") + 1;
        let span = Span::new(cursor, cursor);

        let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(db);
        let ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), ProjectId::new(0));

        let actions = ide.code_actions_lsp(CancellationToken::new(), file, Some(span));
        let has_quick_fix = actions.iter().any(|action| match action {
            lsp_types::CodeActionOrCommand::CodeAction(action) => {
                action.kind == Some(lsp_types::CodeActionKind::QUICKFIX)
                    && action.title == "Cast to int"
            }
            _ => false,
        });

        assert!(
            has_quick_fix,
            "expected quick fix action for type mismatch; got {actions:?}"
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

    #[test]
    fn framework_analyzer_adapter_is_applicable_is_fast_and_does_not_call_applies_to() {
        struct SlowAppliesToAnalyzer {
            calls: Arc<AtomicUsize>,
        }

        impl FrameworkAnalyzer for SlowAppliesToAnalyzer {
            fn applies_to(&self, _db: &dyn Database, _project: ProjectId) -> bool {
                self.calls.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(100));
                true
            }

            fn diagnostics(&self, _db: &dyn Database, _file: nova_ext::FileId) -> Vec<Diagnostic> {
                vec![Diagnostic::warning(
                    "FW",
                    "framework",
                    Some(Span::new(0, 1)),
                )]
            }
        }

        let mut db = MemoryDatabase::new();
        let project = db.add_project();
        let file = db.add_file(project);

        let db: Arc<dyn Database + Send + Sync> = Arc::new(db);
        let calls = Arc::new(AtomicUsize::new(0));

        let adapter = FrameworkAnalyzerAdapter::new(
            "framework.slow",
            SlowAppliesToAnalyzer {
                calls: calls.clone(),
            },
        )
        .into_arc();

        let ctx = ExtensionContext::new(
            Arc::clone(&db),
            Arc::new(NovaConfig::default()),
            project,
            CancellationToken::new(),
        );

        let provider: &dyn DiagnosticProvider<dyn Database + Send + Sync> = adapter.as_ref();
        let applicable = provider.is_applicable(&ctx);

        assert!(applicable, "expected optimistic applicability");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "is_applicable must not call FrameworkAnalyzer::applies_to"
        );

        // Ensure the slow `applies_to` call happens under the registry watchdog rather than
        // blocking `is_applicable`.
        let mut registry: ExtensionRegistry<dyn Database + Send + Sync> =
            ExtensionRegistry::default();
        registry.options_mut().diagnostic_timeout = Duration::from_millis(10);
        registry
            .register_diagnostic_provider(adapter.clone())
            .unwrap();

        let start = Instant::now();
        let out = registry.diagnostics(ctx, DiagnosticParams { file });
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_millis(80),
            "expected watchdog timeout to bound latency; elapsed={elapsed:?}"
        );
        assert!(
            out.is_empty(),
            "slow applies_to should cause a timeout and contribute no diagnostics"
        );

        // Allow any in-flight `applies_to` call to finish so other tests don't inherit wedged
        // watchdog workers.
        std::thread::sleep(Duration::from_millis(120));
    }

    #[test]
    fn framework_analyzer_adapter_applies_to_panics_are_trapped_and_cached_as_inapplicable() {
        struct PanickingAppliesToAnalyzer {
            calls: Arc<AtomicUsize>,
        }

        impl FrameworkAnalyzer for PanickingAppliesToAnalyzer {
            fn applies_to(&self, _db: &dyn Database, _project: ProjectId) -> bool {
                self.calls.fetch_add(1, Ordering::SeqCst);
                panic!("boom");
            }

            fn diagnostics(&self, _db: &dyn Database, _file: nova_ext::FileId) -> Vec<Diagnostic> {
                vec![Diagnostic::warning(
                    "FW",
                    "framework",
                    Some(Span::new(0, 1)),
                )]
            }
        }

        let mut db = MemoryDatabase::new();
        let project = db.add_project();
        let file = db.add_file(project);

        let db: Arc<dyn Database + Send + Sync> = Arc::new(db);
        let calls = Arc::new(AtomicUsize::new(0));

        let adapter = FrameworkAnalyzerAdapter::new(
            "framework.panic",
            PanickingAppliesToAnalyzer {
                calls: calls.clone(),
            },
        )
        .into_arc();

        let mut registry: ExtensionRegistry<dyn Database + Send + Sync> =
            ExtensionRegistry::default();
        registry
            .register_diagnostic_provider(adapter.clone())
            .unwrap();

        let ctx = ExtensionContext::new(
            Arc::clone(&db),
            Arc::new(NovaConfig::default()),
            project,
            CancellationToken::new(),
        );

        // First call: optimistic `is_applicable` allows provider invocation, which should trap the
        // panic and treat the analyzer as inapplicable.
        let out = registry.diagnostics(
            ExtensionContext::new(
                Arc::clone(&db),
                Arc::new(NovaConfig::default()),
                project,
                CancellationToken::new(),
            ),
            DiagnosticParams { file },
        );
        assert!(out.is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Second call: applicability should now be cached, so the provider is filtered out before
        // the watchdog path runs.
        let out2 = registry.diagnostics(ctx, DiagnosticParams { file });
        assert!(out2.is_empty());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "expected cached applicability to prevent repeated applies_to calls"
        );
    }

    #[test]
    fn type_mismatch_quick_fixes_return_empty_when_cancelled() {
        let uri: lsp_types::Uri = "file:///test.java".parse().unwrap();
        let diagnostics = vec![Diagnostic::error(
            "type-mismatch",
            "type mismatch: expected String, found int",
            Some(Span::new(0, 1)),
        )];
        let selection = Span::new(0, 1);

        let cancel = CancellationToken::new();
        let actions = type_mismatch_quick_fixes(&cancel, "x", &uri, selection, &diagnostics);
        assert!(
            !actions.is_empty(),
            "sanity check: expected quick fixes when not cancelled"
        );

        cancel.cancel();
        let actions = type_mismatch_quick_fixes(&cancel, "x", &uri, selection, &diagnostics);
        assert!(actions.is_empty());
    }
}
