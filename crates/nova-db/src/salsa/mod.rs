//! Salsa-powered incremental query database for Nova.
//!
//! This is the foundation for Nova's incremental computation engine:
//! - input queries for file content/existence and project configuration
//! - derived queries for parsing and per-file structural summaries
//! - snapshot-based concurrency (via `ra_salsa::ParallelDatabase`)
//! - lightweight instrumentation (query timings + optional `tracing`)
//!
//! ## Cancellation
//!
//! Salsa cancellation is cooperative: a query will only stop once it reaches a
//! cancellation checkpoint (`db.unwind_if_cancelled()`).
//!
//! **All queries doing more than ~1ms of work must checkpoint cancellation
//! periodically.** See [`cancellation`] for the recommended helper API/pattern.
//!
//! ## Usage sketch
//!
//! ```rust
//! use nova_db::salsa::NovaSyntax;
//! use nova_db::{FileId, SalsaDatabase};
//!
//! let db = SalsaDatabase::new();
//! let file = FileId::from_raw(0);
//! db.set_file_text(file, "class Foo {}".to_string());
//!
//! let snap = db.snapshot();
//! let parse = snap.parse(file);
//! assert!(parse.errors.is_empty());
//! ```

mod cancellation;
mod class_ids;
mod diagnostics;
mod flow;
mod hir;
mod ide;
mod indexing;
mod inputs;
mod interned_class_key;
mod item_tree_store;
mod java_parse_cache;
mod jpms;
mod resolve;
mod semantic;
mod stats;
mod syntax;
mod typeck;
mod workspace;

pub use class_ids::{ClassIdInterner, ClassKey, HasClassInterner};
pub use diagnostics::NovaDiagnostics;
pub use flow::NovaFlow;
pub use hir::NovaHir;
pub use ide::NovaIde;
pub use indexing::NovaIndexing;
pub use inputs::NovaInputs;
pub use interned_class_key::{InternedClassKey, InternedClassKeyId, NovaInternedClassKeys};
pub use item_tree_store::ItemTreeStore;
pub use resolve::{NovaResolve, WorkspaceClassIdMap};
pub use semantic::NovaSemantic;
pub use stats::{HasQueryStats, QueryStat, QueryStatReport, QueryStats, QueryStatsReport};
pub use syntax::{NovaSyntax, SyntaxTree};
pub use typeck::{BodyTypeckResult, DemandExprTypeckResult, FileExprId, NovaTypeck};
pub use workspace::{WorkspaceLoadError, WorkspaceLoader};

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use parking_lot::Mutex as ParkingMutex;
use parking_lot::RwLock;

use nova_core::ClassId;
use nova_core::ProjectDatabase;
use nova_memory::{
    EvictionRequest, EvictionResult, MemoryCategory, MemoryEvictor, MemoryManager, MemoryPressure,
};
use nova_project::{BuildSystem, JavaConfig, JavaVersion, ProjectConfig};
use nova_resolve::ids::DefWithBodyId;
use nova_syntax::{SyntaxTreeStore, TextEdit};
use nova_vfs::OpenDocuments;

use crate::persistence::{HasPersistence, Persistence, PersistenceConfig};
use crate::{FileId, ProjectId, SourceRootId};

use self::stats::QueryStatsCollector;
use java_parse_cache::JavaParseCache;

/// `Arc` wrapper that compares by pointer identity.
///
/// This is used for Salsa inputs that are expensive to compare structurally
/// (e.g. classpath/JDK indexes). The host is responsible for replacing the
/// `Arc` whenever the underlying data changes.
pub struct ArcEq<T: ?Sized>(pub Arc<T>);

impl<T: ?Sized> Clone for ArcEq<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T: ?Sized> ArcEq<T> {
    pub fn new(value: Arc<T>) -> Self {
        Self(value)
    }
}

impl<T: ?Sized> std::ops::Deref for ArcEq<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.0.as_ref()
    }
}

impl<T: ?Sized> PartialEq for ArcEq<T> {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl<T: ?Sized> Eq for ArcEq<T> {}

impl<T: ?Sized> fmt::Debug for ArcEq<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ArcEq").field(&Arc::as_ptr(&self.0)).finish()
    }
}

impl<T: ?Sized> From<Arc<T>> for ArcEq<T> {
    fn from(value: Arc<T>) -> Self {
        Self(value)
    }
}

thread_local! {
    static QUERY_NAME_BUFFER: RefCell<String> = RefCell::new(String::with_capacity(64));
}

/// Writes a `Debug` representation into a string but stops at the first `(`.
///
/// Salsa's `DatabaseKeyIndex::debug(db)` format starts with the query name and
/// then prints arguments in parentheses (`query(key)`); we only need the query
/// name to attribute events, so we intentionally abort formatting early to keep
/// overhead low.
struct QueryNameWriter<'a> {
    out: &'a mut String,
    done: bool,
}

impl fmt::Write for QueryNameWriter<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self.done {
            return Err(fmt::Error);
        }

        if let Some(idx) = s.find('(') {
            self.out.push_str(&s[..idx]);
            self.done = true;
            return Err(fmt::Error);
        }

        self.out.push_str(s);
        Ok(())
    }
}

fn with_query_name<R>(
    db: &RootDatabase,
    database_key: ra_salsa::DatabaseKeyIndex,
    f: impl FnOnce(&str) -> R,
) -> R {
    QUERY_NAME_BUFFER.with(|buf| {
        let mut buf = buf.borrow_mut();
        buf.clear();

        let mut writer = QueryNameWriter {
            out: &mut buf,
            done: false,
        };

        // NOTE: `QueryNameWriter` uses `fmt::Error` as a "stop early" signal
        // once it sees `(`. The formatting machinery treats this as an error;
        // we intentionally ignore it and use whatever prefix was collected.
        let _ = fmt::write(&mut writer, format_args!("{:?}", database_key.debug(db)));

        let raw = buf.trim();
        let name = raw.rsplit("::").next().unwrap_or(raw);
        f(name)
    })
}

/// Best-effort file path lookup used for persistence keys.
///
/// This is intentionally *not* tracked by Salsa: file paths should never affect
/// the semantic output of queries (only the ability to warm-start from disk).
pub trait HasFilePaths {
    fn file_path(&self, file: FileId) -> Option<Arc<String>>;
}

/// Best-effort in-memory pinning for expensive `item_tree` results for open docs.
///
/// This is not tracked by Salsa: it is a pure performance optimization and must
/// never affect query semantics.
pub trait HasItemTreeStore {
    fn item_tree_store(&self) -> Option<Arc<ItemTreeStore>>;
}

/// Accessor for Nova's (optional) shared [`nova_syntax::SyntaxTreeStore`].
///
/// When present, Salsa query implementations may use the store to *pin* syntax
/// trees for open documents and/or reuse trees across memo eviction.
pub trait HasSyntaxTreeStore {
    fn syntax_tree_store(&self) -> Option<Arc<SyntaxTreeStore>>;
}

/// Accessor for the optional open-document Java parse store.
///
/// This is intentionally outside Salsa's dependency tracking: it's a best-effort
/// cache used to pin expensive-to-build parse trees for open documents across
/// Salsa memo eviction.
pub trait HasJavaParseStore {
    fn java_parse_store(&self) -> Option<Arc<nova_syntax::JavaParseStore>>;
}

/// File-keyed memoized query results tracked for memory accounting.
///
/// This is intentionally coarse: it is used to approximate the footprint of
/// Salsa memo tables which can otherwise grow without bound in large workspaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrackedSalsaMemo {
    /// Green-tree parse results for [`NovaSyntax::parse`].
    ///
    /// When a parse result is pinned in [`nova_syntax::SyntaxTreeStore`] (e.g.
    /// for an open document), the `Arc<ParseResult>` allocation is shared
    /// between Salsa memo tables and the syntax tree store. In that case we
    /// record `0` bytes here to avoid double-counting; the bytes are instead
    /// accounted under [`MemoryCategory::SyntaxTrees`].
    Parse,
    /// Full-fidelity Rowan Java parse results for [`NovaSyntax::parse_java`].
    ///
    /// When a parse result is pinned in [`nova_syntax::JavaParseStore`] (e.g. for
    /// an open document), the `Arc<JavaParseResult>` allocation is shared between
    /// Salsa memo tables and the store. In that case we record `0` bytes here to
    /// avoid double-counting; the bytes are instead accounted under
    /// [`MemoryCategory::SyntaxTrees`].
    ParseJava,
    /// Lightweight Java AST produced by [`NovaHir::java_parse`].
    JavaParse,
    /// Stable mapping between syntax nodes and per-file [`nova_hir::ast_id::AstId`]s produced by
    /// [`NovaHir::hir_ast_id_map`].
    HirAstIdMap,
    /// Token-based structural summary for [`NovaSemantic::item_tree`].
    ///
    /// When an `item_tree` result is pinned in [`ItemTreeStore`] (e.g. for an
    /// open document), the `Arc<TokenItemTree>` allocation is shared between
    /// Salsa memo tables and the store. In that case we record `0` bytes here
    /// to avoid double-counting; the bytes are instead accounted under
    /// [`MemoryCategory::SyntaxTrees`].
    ItemTree,
    /// File-level HIR item tree produced by [`NovaHir::hir_item_tree`].
    HirItemTree,
    /// File-level scope graph produced by [`NovaResolve::scope_graph`].
    ///
    /// This can be large in workspaces where name resolution is triggered across
    /// many files (e.g. during analysis/typeck), so we track it separately from
    /// the HIR item tree.
    ScopeGraph,
    /// File-level definition map produced by [`NovaResolve::def_map`].
    ///
    /// This can be large in projects with many types and members, so we account
    /// for it separately from the HIR item tree.
    DefMap,
    /// File-level import map produced by [`NovaResolve::import_map`].
    ImportMap,
    /// Per-file `ProjectIndexes` fragment produced by [`NovaIndexing::file_index_delta`].
    ///
    /// This can be large in projects with many symbols and references, so we
    /// track it separately from parse and item tree memos.
    FileIndexDelta,
}

/// Project-keyed memoized query results tracked for memory accounting.
///
/// These can be large, especially on warm-start where the persisted shards are
/// loaded from disk and only a small number of files are reindexed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrackedSalsaProjectMemo {
    /// Sharded project indexes produced by [`NovaIndexing::project_index_shards`].
    ProjectIndexShards,
    /// Merged project indexes produced by [`NovaIndexing::project_indexes`].
    ProjectIndexes,
    /// Workspace-wide type namespace for a project produced by [`NovaResolve::workspace_def_map`].
    WorkspaceDefMap,
    /// Project-scoped base `TypeStore` produced by [`NovaTypeck::project_base_type_store`].
    ProjectBaseTypeStore,
}

/// Body-keyed memoized query results tracked for memory accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrackedSalsaBodyMemo {
    /// HIR body lowered for a single method/constructor/initializer.
    HirBody,
    /// Flow-oriented body lowered for control-flow analysis.
    FlowBody,
    /// Control-flow graph produced by [`NovaFlow::cfg`].
    Cfg,
    /// Lexical scopes for a single body produced by [`NovaTypeck::expr_scopes`].
    ExprScopes,
    /// Type-checking results for a single body produced by [`NovaTypeck::typeck_body`].
    TypeckBody,
}

/// Database functionality needed by query implementations to record memo sizes.
///
/// Implementations should treat the values as best-effort hints and must not
/// panic if accounting fails.
pub trait HasSalsaMemoStats {
    fn record_salsa_memo_bytes(&self, file: FileId, memo: TrackedSalsaMemo, bytes: u64);

    fn record_salsa_body_memo_bytes(
        &self,
        _owner: DefWithBodyId,
        _memo: TrackedSalsaBodyMemo,
        _bytes: u64,
    ) {
    }

    fn record_salsa_project_memo_bytes(
        &self,
        _project: ProjectId,
        _memo: TrackedSalsaProjectMemo,
        _bytes: u64,
    ) {
    }
}

/// Database functionality needed by `parse_java` to access the previous parse result for
/// incremental reparsing.
pub trait HasJavaParseCache {
    fn java_parse_cache(&self) -> &JavaParseCache;
}

#[derive(Debug, Default)]
struct SalsaMemoFootprint {
    inner: Mutex<SalsaMemoFootprintInner>,
    tracker: OnceLock<nova_memory::MemoryTracker>,
}

#[derive(Debug, Default)]
struct SalsaInputFootprint {
    inner: Mutex<SalsaInputFootprintInner>,
    tracker: OnceLock<nova_memory::MemoryTracker>,
    registration: OnceLock<nova_memory::MemoryRegistration>,
}

#[derive(Debug, Default)]
struct SalsaMemoFootprintInner {
    by_file: HashMap<FileId, FileMemoBytes>,
    by_project: HashMap<ProjectId, ProjectMemoBytes>,
    by_body: HashMap<DefWithBodyId, BodyMemoBytes>,
    total_bytes: u64,
}

#[derive(Debug, Default)]
struct SalsaInputFootprintInner {
    file_text_by_file: HashMap<FileId, FileTextBytes>,
    file_rel_path_by_file: HashMap<FileId, u64>,
    all_file_ids_bytes: u64,
    project_config_by_project: HashMap<ProjectId, u64>,
    project_files_by_project: HashMap<ProjectId, u64>,
    project_class_ids_by_project: HashMap<ProjectId, u64>,
    total_bytes: u64,
}

#[derive(Debug, Clone, Copy, Default)]
struct FileTextBytes {
    content_ptr: usize,
    content_len: u64,
    prev_content_ptr: usize,
    prev_content_len: u64,
    last_edit_len: u64,
}

impl FileTextBytes {
    fn total(self) -> u64 {
        let mut bytes = self.content_len;
        if self.prev_content_ptr != self.content_ptr {
            bytes = bytes.saturating_add(self.prev_content_len);
        }
        bytes.saturating_add(self.last_edit_len)
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct FileMemoBytes {
    /// Bytes recorded for the `parse` memo (None if the query has never been executed).
    parse: Option<u64>,
    /// Bytes recorded for the `parse_java` memo (None if the query has never been executed).
    parse_java: Option<u64>,
    /// Bytes recorded for the `java_parse` memo.
    java_parse: u64,
    /// Bytes recorded for the `hir_ast_id_map` memo.
    hir_ast_id_map: u64,
    /// Bytes recorded for the `item_tree` memo (None if the query has never been executed).
    item_tree: Option<u64>,
    /// Bytes recorded for the `hir_item_tree` memo.
    hir_item_tree: u64,
    /// Bytes recorded for the `scope_graph` memo.
    scope_graph: u64,
    /// Bytes recorded for the `def_map` memo.
    def_map: u64,
    /// Bytes recorded for the `import_map` memo.
    import_map: u64,
    file_index_delta: u64,
}

#[derive(Debug, Clone, Copy, Default)]
struct ProjectMemoBytes {
    project_index_shards: u64,
    project_indexes: u64,
    workspace_def_map: u64,
    project_base_type_store: u64,
}

impl ProjectMemoBytes {
    fn total(self) -> u64 {
        self.project_index_shards
            + self.project_indexes
            + self.workspace_def_map
            + self.project_base_type_store
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct BodyMemoBytes {
    hir_body: u64,
    flow_body: u64,
    cfg: u64,
    expr_scopes: u64,
    typeck_body: u64,
}

impl BodyMemoBytes {
    fn total(self) -> u64 {
        self.hir_body + self.flow_body + self.cfg + self.expr_scopes + self.typeck_body
    }
}

impl FileMemoBytes {
    fn total(self) -> u64 {
        self.parse.unwrap_or(0)
            + self.parse_java.unwrap_or(0)
            + self.java_parse
            + self.hir_ast_id_map
            + self.item_tree.unwrap_or(0)
            + self.hir_item_tree
            + self.scope_graph
            + self.def_map
            + self.import_map
            + self.file_index_delta
    }
}

impl SalsaMemoFootprint {
    fn lock_inner(&self) -> std::sync::MutexGuard<'_, SalsaMemoFootprintInner> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn bind_tracker(&self, tracker: nova_memory::MemoryTracker) {
        let _ = self.tracker.set(tracker);
        self.refresh_tracker();
    }

    fn refresh_tracker(&self) {
        let Some(tracker) = self.tracker.get() else {
            return;
        };
        let bytes = self.lock_inner().total_bytes;
        tracker.set_bytes(bytes);
    }

    fn bytes(&self) -> u64 {
        self.lock_inner().total_bytes
    }

    fn clear(&self) {
        let mut inner = self.lock_inner();
        inner.by_file.clear();
        inner.by_project.clear();
        inner.by_body.clear();
        inner.total_bytes = 0;
        drop(inner);
        self.refresh_tracker();
    }

    fn record(&self, file: FileId, memo: TrackedSalsaMemo, bytes: u64) {
        let mut inner = self.lock_inner();
        let entry = inner.by_file.entry(file).or_default();
        let prev_total = entry.total();

        match memo {
            TrackedSalsaMemo::Parse => entry.parse = Some(bytes),
            TrackedSalsaMemo::ParseJava => entry.parse_java = Some(bytes),
            TrackedSalsaMemo::JavaParse => entry.java_parse = bytes,
            TrackedSalsaMemo::HirAstIdMap => entry.hir_ast_id_map = bytes,
            TrackedSalsaMemo::ItemTree => entry.item_tree = Some(bytes),
            TrackedSalsaMemo::HirItemTree => entry.hir_item_tree = bytes,
            TrackedSalsaMemo::ScopeGraph => entry.scope_graph = bytes,
            TrackedSalsaMemo::DefMap => entry.def_map = bytes,
            TrackedSalsaMemo::ImportMap => entry.import_map = bytes,
            TrackedSalsaMemo::FileIndexDelta => entry.file_index_delta = bytes,
        }

        let next_total = entry.total();
        inner.total_bytes = inner
            .total_bytes
            .saturating_sub(prev_total)
            .saturating_add(next_total);
        drop(inner);
        self.refresh_tracker();
    }

    fn record_project(&self, project: ProjectId, memo: TrackedSalsaProjectMemo, bytes: u64) {
        let mut inner = self.lock_inner();
        let entry = inner.by_project.entry(project).or_default();
        let prev_total = entry.total();

        match memo {
            TrackedSalsaProjectMemo::ProjectIndexShards => entry.project_index_shards = bytes,
            TrackedSalsaProjectMemo::ProjectIndexes => entry.project_indexes = bytes,
            TrackedSalsaProjectMemo::WorkspaceDefMap => entry.workspace_def_map = bytes,
            TrackedSalsaProjectMemo::ProjectBaseTypeStore => entry.project_base_type_store = bytes,
        }

        let next_total = entry.total();
        inner.total_bytes = inner
            .total_bytes
            .saturating_sub(prev_total)
            .saturating_add(next_total);
        drop(inner);
        self.refresh_tracker();
    }

    fn record_body(&self, owner: DefWithBodyId, memo: TrackedSalsaBodyMemo, bytes: u64) {
        let mut inner = self.lock_inner();
        let entry = inner.by_body.entry(owner).or_default();
        let prev_total = entry.total();

        match memo {
            TrackedSalsaBodyMemo::HirBody => entry.hir_body = bytes,
            TrackedSalsaBodyMemo::FlowBody => entry.flow_body = bytes,
            TrackedSalsaBodyMemo::Cfg => entry.cfg = bytes,
            TrackedSalsaBodyMemo::ExprScopes => entry.expr_scopes = bytes,
            TrackedSalsaBodyMemo::TypeckBody => entry.typeck_body = bytes,
        }

        let next_total = entry.total();
        inner.total_bytes = inner
            .total_bytes
            .saturating_sub(prev_total)
            .saturating_add(next_total);
        drop(inner);
        self.refresh_tracker();
    }
}

impl SalsaInputFootprint {
    fn lock_inner(&self) -> std::sync::MutexGuard<'_, SalsaInputFootprintInner> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn bind_tracker(&self, tracker: nova_memory::MemoryTracker) {
        let _ = self.tracker.set(tracker);
        self.refresh_tracker();
    }

    fn refresh_tracker(&self) {
        let Some(tracker) = self.tracker.get() else {
            return;
        };
        let bytes = self.lock_inner().total_bytes;
        tracker.set_bytes(bytes);
    }

    fn bytes(&self) -> u64 {
        self.lock_inner().total_bytes
    }

    fn register(&self, manager: &MemoryManager) {
        if self.registration.get().is_some() {
            return;
        }

        // Salsa file texts are *inputs* (not caches) and are effectively
        // non-evictable: they must remain available to compute query results.
        //
        // We track them under `Other` rather than `QueryCache` to avoid
        // conflating inputs with memoized results, but still want them to drive
        // eviction of query caches/memos under high total pressure. The memory
        // manager is responsible for compensating across categories when
        // non-evictable inputs dominate.
        let registration =
            manager.register_tracker("salsa_inputs".to_string(), MemoryCategory::Other);
        self.bind_tracker(registration.tracker());
        let _ = self.registration.set(registration);
    }

    fn record_file_text(
        &self,
        file: FileId,
        content: &Arc<String>,
        prev_content: &Arc<String>,
        last_edit: Option<&TextEdit>,
    ) {
        let next = FileTextBytes {
            content_ptr: Arc::as_ptr(content) as usize,
            content_len: content.len() as u64,
            prev_content_ptr: Arc::as_ptr(prev_content) as usize,
            prev_content_len: prev_content.len() as u64,
            last_edit_len: last_edit
                .map(|edit| edit.replacement.len() as u64)
                .unwrap_or(0),
        };

        let mut inner = self.lock_inner();
        let prev_total = inner
            .file_text_by_file
            .get(&file)
            .copied()
            .map(FileTextBytes::total)
            .unwrap_or(0);
        let next_total = next.total();
        inner.file_text_by_file.insert(file, next);
        inner.total_bytes = inner
            .total_bytes
            .saturating_sub(prev_total)
            .saturating_add(next_total);
        drop(inner);
        self.refresh_tracker();
    }

    fn record_file_rel_path_len(&self, file: FileId, len: u64) {
        let mut inner = self.lock_inner();
        let prev = inner.file_rel_path_by_file.insert(file, len).unwrap_or(0);
        inner.total_bytes = inner.total_bytes.saturating_sub(prev).saturating_add(len);
        drop(inner);
        self.refresh_tracker();
    }

    fn record_all_file_ids_bytes(&self, bytes: u64) {
        let mut inner = self.lock_inner();
        let prev = inner.all_file_ids_bytes;
        inner.all_file_ids_bytes = bytes;
        inner.total_bytes = inner.total_bytes.saturating_sub(prev).saturating_add(bytes);
        drop(inner);
        self.refresh_tracker();
    }

    fn record_project_config_bytes(&self, project: ProjectId, bytes: u64) {
        let mut inner = self.lock_inner();
        let prev = inner
            .project_config_by_project
            .insert(project, bytes)
            .unwrap_or(0);
        inner.total_bytes = inner.total_bytes.saturating_sub(prev).saturating_add(bytes);
        drop(inner);
        self.refresh_tracker();
    }

    fn record_project_files_bytes(&self, project: ProjectId, bytes: u64) {
        let mut inner = self.lock_inner();
        let prev = inner
            .project_files_by_project
            .insert(project, bytes)
            .unwrap_or(0);
        inner.total_bytes = inner.total_bytes.saturating_sub(prev).saturating_add(bytes);
        drop(inner);
        self.refresh_tracker();
    }

    fn record_project_class_ids_bytes(&self, project: ProjectId, bytes: u64) {
        let mut inner = self.lock_inner();
        let prev = inner
            .project_class_ids_by_project
            .insert(project, bytes)
            .unwrap_or(0);
        inner.total_bytes = inner.total_bytes.saturating_sub(prev).saturating_add(bytes);
        drop(inner);
        self.refresh_tracker();
    }
}

fn estimated_project_config_bytes(config: &ProjectConfig) -> u64 {
    use std::mem::size_of;
    use std::path::Path;

    fn add_bytes(total: &mut u64, bytes: u64) {
        *total = total.saturating_add(bytes);
    }

    fn add_slice<T>(total: &mut u64, slice: &[T]) {
        add_bytes(
            total,
            (slice.len() as u64).saturating_mul(size_of::<T>() as u64),
        );
    }

    fn add_path(total: &mut u64, path: &Path) {
        add_bytes(total, path.as_os_str().len() as u64);
    }

    fn add_pathbuf(total: &mut u64, path: &PathBuf) {
        add_path(total, path.as_path());
    }

    fn add_opt_pathbuf(total: &mut u64, path: &Option<PathBuf>) {
        if let Some(path) = path {
            add_pathbuf(total, path);
        }
    }

    fn add_string(total: &mut u64, s: &String) {
        add_bytes(total, s.len() as u64);
    }

    fn add_opt_string(total: &mut u64, s: &Option<String>) {
        if let Some(s) = s {
            add_string(total, s);
        }
    }

    fn add_module_name(total: &mut u64, name: &nova_modules::ModuleName) {
        add_bytes(total, name.as_str().len() as u64);
    }

    fn annotation_processing_config_bytes(cfg: &nova_project::AnnotationProcessingConfig) -> u64 {
        let mut bytes = 0u64;
        add_opt_pathbuf(&mut bytes, &cfg.generated_sources_dir);
        add_slice(&mut bytes, cfg.processor_path.as_slice());
        for path in &cfg.processor_path {
            add_pathbuf(&mut bytes, path);
        }
        add_slice(&mut bytes, cfg.processors.as_slice());
        for proc in &cfg.processors {
            add_string(&mut bytes, proc);
        }
        add_slice(&mut bytes, cfg.compiler_args.as_slice());
        for arg in &cfg.compiler_args {
            add_string(&mut bytes, arg);
        }

        // Best-effort: count key/value string lengths (ignore BTreeMap node overhead).
        add_bytes(
            &mut bytes,
            (cfg.options.len() as u64).saturating_mul(size_of::<(String, String)>() as u64),
        );
        for (k, v) in &cfg.options {
            add_string(&mut bytes, k);
            add_string(&mut bytes, v);
        }

        bytes
    }

    fn annotation_processing_bytes(ap: &nova_project::AnnotationProcessing) -> u64 {
        let mut bytes = 0u64;
        if let Some(cfg) = &ap.main {
            bytes = bytes.saturating_add(annotation_processing_config_bytes(cfg));
        }
        if let Some(cfg) = &ap.test {
            bytes = bytes.saturating_add(annotation_processing_config_bytes(cfg));
        }
        bytes
    }

    fn source_root_bytes(root: &nova_project::SourceRoot) -> u64 {
        let mut bytes = 0u64;
        add_pathbuf(&mut bytes, &root.path);
        bytes
    }

    fn classpath_entry_bytes(entry: &nova_project::ClasspathEntry) -> u64 {
        let mut bytes = 0u64;
        add_pathbuf(&mut bytes, &entry.path);
        bytes
    }

    fn output_dir_bytes(dir: &nova_project::OutputDir) -> u64 {
        let mut bytes = 0u64;
        add_pathbuf(&mut bytes, &dir.path);
        bytes
    }

    fn dependency_bytes(dep: &nova_project::Dependency) -> u64 {
        let mut bytes = 0u64;
        add_string(&mut bytes, &dep.group_id);
        add_string(&mut bytes, &dep.artifact_id);
        add_opt_string(&mut bytes, &dep.version);
        add_opt_string(&mut bytes, &dep.scope);
        add_opt_string(&mut bytes, &dep.classifier);
        add_opt_string(&mut bytes, &dep.type_);
        bytes
    }

    fn module_bytes(module: &nova_project::Module) -> u64 {
        let mut bytes = 0u64;
        add_string(&mut bytes, &module.name);
        add_pathbuf(&mut bytes, &module.root);
        bytes = bytes.saturating_add(annotation_processing_bytes(&module.annotation_processing));
        bytes
    }

    fn module_info_bytes(info: &nova_modules::ModuleInfo) -> u64 {
        let mut bytes = 0u64;
        add_module_name(&mut bytes, &info.name);

        add_slice(&mut bytes, info.requires.as_slice());
        for req in &info.requires {
            add_module_name(&mut bytes, &req.module);
        }

        add_slice(&mut bytes, info.exports.as_slice());
        for export in &info.exports {
            add_string(&mut bytes, &export.package);
            add_slice(&mut bytes, export.to.as_slice());
            for name in &export.to {
                add_module_name(&mut bytes, name);
            }
        }

        add_slice(&mut bytes, info.opens.as_slice());
        for open in &info.opens {
            add_string(&mut bytes, &open.package);
            add_slice(&mut bytes, open.to.as_slice());
            for name in &open.to {
                add_module_name(&mut bytes, name);
            }
        }

        add_slice(&mut bytes, info.uses.as_slice());
        for uses in &info.uses {
            add_string(&mut bytes, &uses.service);
        }

        add_slice(&mut bytes, info.provides.as_slice());
        for provides in &info.provides {
            add_string(&mut bytes, &provides.service);
            add_slice(&mut bytes, provides.implementations.as_slice());
            for impl_ in &provides.implementations {
                add_string(&mut bytes, impl_);
            }
        }

        bytes
    }

    fn module_graph_bytes(graph: &nova_modules::ModuleGraph) -> u64 {
        let mut bytes = 0u64;
        for (name, info) in graph.iter() {
            // Best-effort container sizing.
            bytes = bytes
                .saturating_add(
                    size_of::<(nova_modules::ModuleName, nova_modules::ModuleInfo)>() as u64,
                );
            add_module_name(&mut bytes, name);
            bytes = bytes.saturating_add(module_info_bytes(info));
        }
        bytes
    }

    fn jpms_module_root_bytes(root: &nova_project::JpmsModuleRoot) -> u64 {
        let mut bytes = 0u64;
        add_module_name(&mut bytes, &root.name);
        add_pathbuf(&mut bytes, &root.root);
        add_pathbuf(&mut bytes, &root.module_info);
        bytes = bytes.saturating_add(module_info_bytes(&root.info));
        bytes
    }

    fn jpms_workspace_bytes(workspace: &nova_project::JpmsWorkspace) -> u64 {
        let mut bytes = 0u64;
        bytes = bytes.saturating_add(module_graph_bytes(&workspace.graph));
        add_bytes(
            &mut bytes,
            (workspace.module_roots.len() as u64)
                .saturating_mul(size_of::<(nova_modules::ModuleName, PathBuf)>() as u64),
        );
        for (name, path) in &workspace.module_roots {
            add_module_name(&mut bytes, name);
            add_pathbuf(&mut bytes, path);
        }
        bytes
    }

    fn module_config_bytes(cfg: &nova_project::ModuleConfig) -> u64 {
        let mut bytes = 0u64;
        add_string(&mut bytes, &cfg.id);

        add_slice(&mut bytes, cfg.source_roots.as_slice());
        for root in &cfg.source_roots {
            bytes = bytes.saturating_add(source_root_bytes(root));
        }

        add_slice(&mut bytes, cfg.classpath.as_slice());
        for entry in &cfg.classpath {
            bytes = bytes.saturating_add(classpath_entry_bytes(entry));
        }

        add_slice(&mut bytes, cfg.module_path.as_slice());
        for entry in &cfg.module_path {
            bytes = bytes.saturating_add(classpath_entry_bytes(entry));
        }

        add_opt_pathbuf(&mut bytes, &cfg.output_dir);

        bytes
    }

    fn workspace_model_bytes(model: &nova_project::WorkspaceModel) -> u64 {
        let mut bytes = 0u64;
        add_slice(&mut bytes, model.modules.as_slice());
        for cfg in &model.modules {
            bytes = bytes.saturating_add(module_config_bytes(cfg));
        }
        bytes
    }

    let mut bytes = size_of::<ProjectConfig>() as u64;
    add_pathbuf(&mut bytes, &config.workspace_root);

    add_slice(&mut bytes, config.modules.as_slice());
    for module in &config.modules {
        bytes = bytes.saturating_add(module_bytes(module));
    }

    add_slice(&mut bytes, config.jpms_modules.as_slice());
    for module in &config.jpms_modules {
        bytes = bytes.saturating_add(jpms_module_root_bytes(module));
    }

    if let Some(workspace) = &config.jpms_workspace {
        bytes = bytes.saturating_add(jpms_workspace_bytes(workspace));
    }

    add_slice(&mut bytes, config.source_roots.as_slice());
    for root in &config.source_roots {
        bytes = bytes.saturating_add(source_root_bytes(root));
    }

    add_slice(&mut bytes, config.module_path.as_slice());
    for entry in &config.module_path {
        bytes = bytes.saturating_add(classpath_entry_bytes(entry));
    }

    add_slice(&mut bytes, config.classpath.as_slice());
    for entry in &config.classpath {
        bytes = bytes.saturating_add(classpath_entry_bytes(entry));
    }

    add_slice(&mut bytes, config.output_dirs.as_slice());
    for dir in &config.output_dirs {
        bytes = bytes.saturating_add(output_dir_bytes(dir));
    }

    add_slice(&mut bytes, config.dependencies.as_slice());
    for dep in &config.dependencies {
        bytes = bytes.saturating_add(dependency_bytes(dep));
    }

    if let Some(model) = &config.workspace_model {
        bytes = bytes.saturating_add(workspace_model_bytes(model));
    }

    bytes
}

#[derive(Debug)]
struct InputIndexTracker {
    name: String,
    inner: Mutex<InputIndexTrackerInner>,
    tracker: OnceLock<nova_memory::MemoryTracker>,
    registration: OnceLock<nova_memory::MemoryRegistration>,
}

#[derive(Debug, Default)]
struct InputIndexTrackerInner {
    /// Project -> tracked value address.
    ///
    /// We store the address as `usize` (rather than a raw pointer) so this tracker remains
    /// `Send`/`Sync` when embedded in the workspace database. The address is only used as an
    /// identity key (to avoid double-counting shared `Arc` allocations) and is never
    /// dereferenced.
    by_project: HashMap<ProjectId, usize>,
    /// Address -> (estimated bytes, number of projects referencing it).
    by_ptr: HashMap<usize, TrackedPtrEntry>,
    total_bytes: u64,
}

#[derive(Debug, Clone, Copy)]
struct TrackedPtrEntry {
    bytes: u64,
    refs: u32,
}

impl InputIndexTracker {
    fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            inner: Mutex::new(InputIndexTrackerInner::default()),
            tracker: OnceLock::new(),
            registration: OnceLock::new(),
        }
    }

    fn lock_inner(&self) -> std::sync::MutexGuard<'_, InputIndexTrackerInner> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn register_evictor(&self, manager: &MemoryManager, evictor: Arc<dyn MemoryEvictor>) {
        if self.registration.get().is_some() {
            return;
        }

        let registration =
            manager.register_evictor(self.name.clone(), MemoryCategory::TypeInfo, evictor);
        let _ = self.tracker.set(registration.tracker());
        let _ = self.registration.set(registration);
        self.refresh_tracker();
    }

    fn refresh_tracker(&self) {
        let Some(tracker) = self.tracker.get() else {
            return;
        };
        let bytes = self.lock_inner().total_bytes;
        tracker.set_bytes(bytes);
    }

    fn bytes(&self) -> u64 {
        self.lock_inner().total_bytes
    }

    fn set_project_ptr(&self, project: ProjectId, ptr: Option<usize>, bytes: u64) {
        let mut inner = self.lock_inner();

        let prev_ptr = inner.by_project.get(&project).copied();
        if prev_ptr == ptr {
            // Same tracked allocation; refresh stored bytes (best-effort) in case
            // the index grew (e.g. lazy caches populated).
            if let Some(ptr) = ptr {
                let update = match inner.by_ptr.get_mut(&ptr) {
                    Some(entry) if entry.bytes != bytes => {
                        let prev = entry.bytes;
                        entry.bytes = bytes;
                        Some((prev, bytes))
                    }
                    _ => None,
                };
                if let Some((prev, next)) = update {
                    inner.total_bytes = inner.total_bytes.saturating_sub(prev).saturating_add(next);
                }
            }
        } else {
            // Drop old ptr (if any).
            if let Some(old_ptr) = prev_ptr {
                inner.by_project.remove(&project);
                let removed_bytes = match inner.by_ptr.get_mut(&old_ptr) {
                    Some(entry) => {
                        entry.refs = entry.refs.saturating_sub(1);
                        (entry.refs == 0).then_some(entry.bytes)
                    }
                    None => None,
                };
                if let Some(removed_bytes) = removed_bytes {
                    inner.total_bytes = inner.total_bytes.saturating_sub(removed_bytes);
                    inner.by_ptr.remove(&old_ptr);
                }
            }

            // Add new ptr (if any).
            if let Some(new_ptr) = ptr {
                inner.by_project.insert(project, new_ptr);
                let mut inserted = false;
                let mut update = None;

                match inner.by_ptr.get_mut(&new_ptr) {
                    Some(entry) => {
                        // Already counted this allocation; just bump ref count and
                        // refresh the estimated size.
                        entry.refs = entry.refs.saturating_add(1);
                        if entry.bytes != bytes {
                            let prev = entry.bytes;
                            entry.bytes = bytes;
                            update = Some((prev, bytes));
                        }
                    }
                    None => {
                        inner
                            .by_ptr
                            .insert(new_ptr, TrackedPtrEntry { bytes, refs: 1 });
                        inserted = true;
                    }
                }

                if inserted {
                    inner.total_bytes = inner.total_bytes.saturating_add(bytes);
                } else if let Some((prev, next)) = update {
                    inner.total_bytes = inner.total_bytes.saturating_sub(prev).saturating_add(next);
                }
            }
        }

        let total = inner.total_bytes;
        drop(inner);

        if let Some(tracker) = self.tracker.get() {
            tracker.set_bytes(total);
        }
    }
}

#[derive(Debug)]
struct JdkIndexEvictor {
    name: String,
    inputs: Arc<ParkingMutex<SalsaInputs>>,
    tracker: Arc<InputIndexTracker>,
}

impl JdkIndexEvictor {
    fn new(inputs: Arc<ParkingMutex<SalsaInputs>>, tracker: Arc<InputIndexTracker>) -> Self {
        Self {
            name: tracker.name.clone(),
            inputs,
            tracker,
        }
    }
}

impl MemoryEvictor for JdkIndexEvictor {
    fn name(&self) -> &str {
        &self.name
    }

    fn category(&self) -> MemoryCategory {
        MemoryCategory::TypeInfo
    }

    fn evict(&self, request: EvictionRequest) -> EvictionResult {
        let before = self.tracker.bytes();
        if before <= request.target_bytes {
            return EvictionResult {
                before_bytes: before,
                after_bytes: before,
            };
        }

        // Under low pressure, avoid disrupting cache locality.
        if matches!(request.pressure, MemoryPressure::Low) {
            return EvictionResult {
                before_bytes: before,
                after_bytes: before,
            };
        }

        let Some(inputs) = self.inputs.try_lock() else {
            return EvictionResult {
                before_bytes: before,
                after_bytes: before,
            };
        };

        let snapshots: Vec<(ProjectId, usize, Arc<nova_jdk::JdkIndex>)> = inputs
            .jdk_index
            .iter()
            .map(|(&project, index)| {
                let index = index.0.clone();
                let ptr = Arc::as_ptr(&index) as usize;
                (project, ptr, index)
            })
            .collect();
        drop(inputs);

        // Clear caches per unique JDK index allocation.
        {
            use std::collections::HashSet;

            let mut seen = HashSet::new();
            for (_project, ptr, index) in &snapshots {
                if seen.insert(*ptr) {
                    index.evict_symbol_caches();
                }
            }
        }

        // Update tracked bytes for projects that still reference the same `Arc` allocation.
        let bytes_by_ptr: HashMap<usize, u64> = snapshots
            .iter()
            .map(|(_project, ptr, index)| (*ptr, index.estimated_bytes()))
            .collect();

        if let Some(inputs) = self.inputs.try_lock() {
            for (project, ptr, _index) in &snapshots {
                let still_same = inputs
                    .jdk_index
                    .get(project)
                    .is_some_and(|current| Arc::as_ptr(&current.0) as usize == *ptr);
                if !still_same {
                    continue;
                }

                let bytes = bytes_by_ptr.get(ptr).copied().unwrap_or(0);
                self.tracker.set_project_ptr(*project, Some(*ptr), bytes);
            }
        }

        let after = self.tracker.bytes();
        EvictionResult {
            before_bytes: before,
            after_bytes: after,
        }
    }
}

struct ClasspathIndexEvictor {
    name: String,
    db: Arc<ParkingMutex<RootDatabase>>,
    inputs: Arc<ParkingMutex<SalsaInputs>>,
    tracker: Arc<InputIndexTracker>,
}

impl ClasspathIndexEvictor {
    fn new(
        db: Arc<ParkingMutex<RootDatabase>>,
        inputs: Arc<ParkingMutex<SalsaInputs>>,
        tracker: Arc<InputIndexTracker>,
    ) -> Self {
        Self {
            name: tracker.name.clone(),
            db,
            inputs,
            tracker,
        }
    }
}

impl MemoryEvictor for ClasspathIndexEvictor {
    fn name(&self) -> &str {
        &self.name
    }

    fn category(&self) -> MemoryCategory {
        MemoryCategory::TypeInfo
    }

    fn evict(&self, request: EvictionRequest) -> EvictionResult {
        let before = self.tracker.bytes();
        if before <= request.target_bytes {
            return EvictionResult {
                before_bytes: before,
                after_bytes: before,
            };
        }

        // Dropping the classpath index is a large UX hit. Only do it when we enter high pressure.
        if !matches!(
            request.pressure,
            MemoryPressure::High | MemoryPressure::Critical
        ) {
            return EvictionResult {
                before_bytes: before,
                after_bytes: before,
            };
        }

        // Lock ordering: `inputs` then `db` (matches the rest of this file).
        let Some(mut inputs) = self.inputs.try_lock() else {
            return EvictionResult {
                before_bytes: before,
                after_bytes: before,
            };
        };
        let Some(mut db) = self.db.try_lock() else {
            return EvictionResult {
                before_bytes: before,
                after_bytes: before,
            };
        };

        let projects: Vec<ProjectId> = inputs
            .classpath_index
            .iter()
            .filter_map(|(&project, index)| index.as_ref().map(|_| project))
            .collect();

        for project in projects {
            inputs.classpath_index.insert(project, None);
            db.set_classpath_index(project, None);
            self.tracker.set_project_ptr(project, None, 0);
        }

        drop(db);
        drop(inputs);

        let after = self.tracker.bytes();
        EvictionResult {
            before_bytes: before,
            after_bytes: after,
        }
    }
}

#[derive(Debug, Default, Clone)]
struct SalsaInputs {
    // File IDs that have a `file_content` value.
    //
    // `ra_salsa` input queries panic when a value hasn't been set, so we must
    // ensure `all_file_ids` only includes file IDs that are safe to query via
    // the per-file text inputs (`file_content`, `file_prev_content`, `file_last_edit`, `file_is_dirty`).
    file_ids: BTreeSet<FileId>,
    file_ids_dirty: bool,
    file_exists: HashMap<FileId, bool>,
    file_project: HashMap<FileId, ProjectId>,
    file_content: HashMap<FileId, Arc<String>>,
    file_prev_content: HashMap<FileId, Arc<String>>,
    file_last_edit: HashMap<FileId, Option<TextEdit>>,
    file_is_dirty: HashMap<FileId, bool>,
    file_rel_path: HashMap<FileId, Arc<String>>,
    source_root: HashMap<FileId, SourceRootId>,
    project_files: HashMap<ProjectId, Arc<Vec<FileId>>>,
    project_config: HashMap<ProjectId, Arc<ProjectConfig>>,
    project_class_ids: HashMap<ProjectId, Arc<Vec<(Arc<str>, ClassId)>>>,
    jdk_index: HashMap<ProjectId, ArcEq<nova_jdk::JdkIndex>>,
    classpath_index: HashMap<ProjectId, Option<ArcEq<nova_classpath::ClasspathIndex>>>,
}

impl SalsaInputs {
    fn apply_to(&self, db: &mut RootDatabase) {
        db.set_all_file_ids(Arc::new(self.file_ids.iter().copied().collect()));
        for (&file, &exists) in &self.file_exists {
            db.set_file_exists(file, exists);
        }
        for (&file, &dirty) in &self.file_is_dirty {
            db.set_file_is_dirty(file, dirty);
        }
        for (&file, &project) in &self.file_project {
            db.set_file_project(file, project);
        }
        for (&file, &root) in &self.source_root {
            db.set_source_root(file, root);
        }
        for (&file, content) in &self.file_content {
            db.set_file_content(file, content.clone());
        }
        for (&file, content) in &self.file_prev_content {
            db.set_file_prev_content(file, content.clone());
        }
        for (&file, edit) in &self.file_last_edit {
            db.set_file_last_edit(file, edit.clone());
        }
        for (&file, path) in &self.file_rel_path {
            db.set_file_rel_path(file, path.clone());
        }
        for (&project, files) in &self.project_files {
            db.set_project_files(project, files.clone());
        }
        for (&project, config) in &self.project_config {
            db.set_project_config(project, config.clone());
        }
        for (&project, mapping) in &self.project_class_ids {
            db.set_project_class_ids(project, mapping.clone());
        }
        for (&project, index) in &self.jdk_index {
            db.set_jdk_index(project, index.clone());
        }
        for (&project, index) in &self.classpath_index {
            db.set_classpath_index(project, index.clone());
        }
    }
}

/// Snapshot of all Salsa interned tables that Nova needs to preserve across
/// `Database::evict_salsa_memos`.
///
/// We currently evict memoized query results by rebuilding the Salsa storage,
/// because `ra_ap_salsa` doesn't expose a safe/stable API to clear memo tables.
/// Rebuilding would ordinarily also drop interned tables (invalidating any
/// `#[ra_salsa::interned]` IDs). To keep interned IDs stable we copy out all
/// interned entries we care about and re-intern them into the fresh database in
/// the original ID order.
///
/// Note: If you add new `#[ra_salsa::interned]` queries to Nova, extend this
/// snapshot so their IDs survive memo eviction as well.
#[derive(Debug, Default)]
struct InternedTablesSnapshot {
    intern_class_keys: Vec<(InternedClassKeyId, InternedClassKey)>,
}

impl InternedTablesSnapshot {
    fn capture(db: &RootDatabase) -> Self {
        use ra_salsa::debug::DebugQueryTable as _;
        use ra_salsa::InternKey as _;

        let mut intern_class_keys: Vec<_> =
            ra_salsa::plumbing::get_query_table::<interned_class_key::InternClassKeyQuery>(db)
                .entries::<Vec<_>>()
                .into_iter()
                .filter_map(|entry| entry.value.map(|id| (id, entry.key)))
                .collect();
        intern_class_keys.sort_by_key(|(id, _)| id.as_intern_id().as_u32());

        Self { intern_class_keys }
    }

    /// Restore the interned entries into `db`.
    ///
    /// Returns `false` if `ra_salsa` assigned a different intern id than expected
    /// (meaning we cannot safely preserve stable identities across eviction).
    fn restore_into(self, db: &RootDatabase) -> bool {
        use self::interned_class_key::NovaInternedClassKeys as _;

        for (expected_id, key) in self.intern_class_keys {
            let actual_id = db.intern_class_key(key);
            if actual_id != expected_id {
                debug_assert_eq!(
                    actual_id, expected_id,
                    "interned ID mismatch while restoring interned tables after memo eviction"
                );
                return false;
            }
        }

        true
    }
}

/// Runs `f` and catches any Salsa cancellation.
///
/// This is a convenience wrapper around `ra_salsa::Cancelled::catch`.
pub fn catch_cancelled<F, T>(f: F) -> Result<T, ra_salsa::Cancelled>
where
    F: FnOnce() -> T,
{
    // `ra_salsa::Cancelled::catch` is based on `catch_unwind`, which requires
    // `UnwindSafe`. Our database carries intentionally non-tracked state used
    // for persistence (locks, disk caches) that is not `UnwindSafe` by default.
    //
    // Cancellation unwinds are expected control-flow in Salsa; queries must be
    // written so that any such unwind results in a benign cache miss on retry.
    ra_salsa::Cancelled::catch(std::panic::AssertUnwindSafe(f))
}
/// Read-only snapshot type for concurrent query execution.
pub type Snapshot = ra_salsa::Snapshot<RootDatabase>;

/// The concrete Salsa database for Nova (the ADR 0001 "RootDatabase").
#[ra_salsa::database(
    inputs::NovaInputsStorage,
    syntax::NovaSyntaxStorage,
    semantic::NovaSemanticStorage,
    flow::NovaFlowStorage,
    hir::NovaHirStorage,
    resolve::NovaResolveStorage,
    typeck::NovaTypeckStorage,
    diagnostics::NovaDiagnosticsStorage,
    ide::NovaIdeStorage,
    indexing::NovaIndexingStorage,
    interned_class_key::NovaInternedClassKeysStorage
)]
pub struct RootDatabase {
    storage: ra_salsa::Storage<RootDatabase>,
    stats: QueryStatsCollector,
    persistence: Persistence,
    file_paths: Arc<RwLock<HashMap<FileId, Arc<String>>>>,
    item_tree_store: Option<Arc<ItemTreeStore>>,
    syntax_tree_store: Option<Arc<SyntaxTreeStore>>,
    class_interner: Arc<ParkingMutex<ClassIdInterner>>,
    memo_footprint: Arc<SalsaMemoFootprint>,
    java_parse_cache: Arc<JavaParseCache>,
    java_parse_store: Option<Arc<nova_syntax::JavaParseStore>>,
}

impl Default for RootDatabase {
    fn default() -> Self {
        let project_root = std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf());
        Self::new_with_persistence(project_root, PersistenceConfig::from_env())
    }
}

impl RootDatabase {
    pub fn new_with_persistence(
        project_root: impl AsRef<Path>,
        persistence: PersistenceConfig,
    ) -> Self {
        let project_root = project_root.as_ref().to_path_buf();
        let mut db = Self {
            storage: ra_salsa::Storage::default(),
            stats: QueryStatsCollector::default(),
            persistence: Persistence::new(&project_root, persistence),
            file_paths: Arc::new(RwLock::new(HashMap::new())),
            item_tree_store: None,
            syntax_tree_store: None,
            class_interner: Arc::new(ParkingMutex::new(ClassIdInterner::default())),
            memo_footprint: Arc::new(SalsaMemoFootprint::default()),
            java_parse_cache: Arc::new(JavaParseCache::default()),
            java_parse_store: None,
        };

        // Provide a sensible default `ProjectConfig` so callers can start
        // asking version-aware questions (like syntax feature diagnostics)
        // without wiring full project discovery first.
        db.set_project_config(
            ProjectId::from_raw(0),
            Arc::new(ProjectConfig {
                workspace_root: project_root,
                build_system: BuildSystem::Simple,
                java: JavaConfig {
                    source: JavaVersion::JAVA_21,
                    target: JavaVersion::JAVA_21,
                    enable_preview: false,
                },
                modules: Vec::new(),
                jpms_modules: Vec::new(),
                jpms_workspace: None,
                source_roots: Vec::new(),
                module_path: Vec::new(),
                classpath: Vec::new(),
                output_dirs: Vec::new(),
                dependencies: Vec::new(),
                workspace_model: None,
            }),
        );

        // Ensure the host-managed class id registry has a sensible default so snapshots can
        // perform lookups without requiring a workspace loader.
        db.set_project_class_ids(ProjectId::from_raw(0), Arc::new(Vec::new()));

        // Ensure "global" inputs have a sensible default so snapshots can be
        // used as a standalone read-only database facade.
        db.set_all_file_ids(Arc::new(Vec::new()));

        db
    }

    pub fn set_file_path(&mut self, file: FileId, path: impl Into<String>) {
        self.set_file_path_arc(file, Arc::new(path.into()));
    }

    pub fn set_file_path_arc(&mut self, file: FileId, path: Arc<String>) {
        self.file_paths.write().insert(file, path);
    }

    pub fn set_java_parse_store(&mut self, store: Option<Arc<nova_syntax::JavaParseStore>>) {
        self.java_parse_store = store;
    }

    /// Set the full text for `file`, initializing all incremental-parse metadata inputs.
    ///
    /// `ra_salsa` input queries panic when unset; callers using `RootDatabase` directly should
    /// prefer this helper over individual setters to avoid missing incremental parsing inputs
    /// like `file_prev_content` / `file_last_edit`.
    pub fn set_file_text_full(&mut self, file: FileId, text: Arc<String>) {
        self.set_file_exists(file, true);
        self.set_file_content(file, text.clone());
        self.set_file_prev_content(file, text);
        self.set_file_last_edit(file, None);
        self.set_file_is_dirty(file, false);
    }

    /// Convenience wrapper around [`RootDatabase::set_file_text_full`].
    pub fn set_file_text(&mut self, file: FileId, text: impl Into<String>) {
        self.set_file_text_full(file, Arc::new(text.into()));
    }

    pub fn persistence_stats(&self) -> crate::PersistenceStats {
        self.persistence.stats()
    }
    /// Request cancellation for in-flight queries.
    ///
    /// Salsa's cancellation is driven by pending writes: this triggers a
    /// "synthetic write" of low durability, which will cause other threads to
    /// unwind at their next `db.unwind_if_cancelled()` checkpoint.
    #[inline]
    pub fn request_cancellation(&mut self) {
        ra_salsa::Database::synthetic_write(self, ra_salsa::Durability::LOW);
    }

    /// Create a read-only snapshot for concurrent query execution.
    ///
    /// This is also available via the `ra_salsa::ParallelDatabase` trait, but
    /// we provide it as an inherent method to avoid requiring a trait import.
    #[inline]
    pub fn snapshot(&self) -> Snapshot {
        ra_salsa::ParallelDatabase::snapshot(self)
    }

    /// Snapshot current query timing stats.
    #[inline]
    pub fn query_stats(&self) -> QueryStats {
        self.stats.snapshot()
    }

    pub fn clear_query_stats(&self) {
        self.stats.clear();
    }
}

impl HasQueryStats for RootDatabase {
    fn record_query_stat(&self, query_name: &'static str, duration: Duration) {
        self.stats.record_time(query_name, duration);
    }

    fn record_disk_cache_hit(&self, query_name: &'static str) {
        self.stats.record_disk_hit(query_name);
    }

    fn record_disk_cache_miss(&self, query_name: &'static str) {
        self.stats.record_disk_miss(query_name);
    }
}

impl HasSalsaMemoStats for RootDatabase {
    fn record_salsa_memo_bytes(&self, file: FileId, memo: TrackedSalsaMemo, bytes: u64) {
        self.memo_footprint.record(file, memo, bytes);
    }

    fn record_salsa_body_memo_bytes(
        &self,
        owner: DefWithBodyId,
        memo: TrackedSalsaBodyMemo,
        bytes: u64,
    ) {
        self.memo_footprint.record_body(owner, memo, bytes);
    }

    fn record_salsa_project_memo_bytes(
        &self,
        project: ProjectId,
        memo: TrackedSalsaProjectMemo,
        bytes: u64,
    ) {
        self.memo_footprint.record_project(project, memo, bytes);
    }
}

impl HasJavaParseCache for RootDatabase {
    fn java_parse_cache(&self) -> &JavaParseCache {
        self.java_parse_cache.as_ref()
    }
}

impl HasJavaParseCache for ra_salsa::Snapshot<RootDatabase> {
    fn java_parse_cache(&self) -> &JavaParseCache {
        std::ops::Deref::deref(self).java_parse_cache.as_ref()
    }
}

impl HasPersistence for RootDatabase {
    fn persistence(&self) -> &Persistence {
        &self.persistence
    }
}

impl HasFilePaths for RootDatabase {
    fn file_path(&self, file: FileId) -> Option<Arc<String>> {
        self.file_paths.read().get(&file).cloned()
    }
}

impl HasItemTreeStore for RootDatabase {
    fn item_tree_store(&self) -> Option<Arc<ItemTreeStore>> {
        self.item_tree_store.clone()
    }
}

impl HasItemTreeStore for ra_salsa::Snapshot<RootDatabase> {
    fn item_tree_store(&self) -> Option<Arc<ItemTreeStore>> {
        std::ops::Deref::deref(self).item_tree_store.clone()
    }
}

impl HasSyntaxTreeStore for RootDatabase {
    fn syntax_tree_store(&self) -> Option<Arc<SyntaxTreeStore>> {
        self.syntax_tree_store.clone()
    }
}

impl HasSyntaxTreeStore for ra_salsa::Snapshot<RootDatabase> {
    fn syntax_tree_store(&self) -> Option<Arc<SyntaxTreeStore>> {
        std::ops::Deref::deref(self).syntax_tree_store.clone()
    }
}

impl HasJavaParseStore for RootDatabase {
    fn java_parse_store(&self) -> Option<Arc<nova_syntax::JavaParseStore>> {
        self.java_parse_store.clone()
    }
}

impl HasJavaParseStore for ra_salsa::Snapshot<RootDatabase> {
    fn java_parse_store(&self) -> Option<Arc<nova_syntax::JavaParseStore>> {
        std::ops::Deref::deref(self).java_parse_store.clone()
    }
}

impl HasClassInterner for RootDatabase {
    fn class_interner(&self) -> &Arc<ParkingMutex<ClassIdInterner>> {
        &self.class_interner
    }
}

impl HasClassInterner for ra_salsa::Snapshot<RootDatabase> {
    fn class_interner(&self) -> &Arc<ParkingMutex<ClassIdInterner>> {
        &std::ops::Deref::deref(self).class_interner
    }
}
impl ra_salsa::Database for RootDatabase {
    fn salsa_event(&self, event: ra_salsa::Event) {
        match event.kind {
            ra_salsa::EventKind::WillExecute { database_key } => {
                with_query_name(self, database_key, |name| self.stats.record_execution(name));
            }
            ra_salsa::EventKind::DidValidateMemoizedValue { database_key } => {
                with_query_name(self, database_key, |name| {
                    self.stats.record_validated_memoized(name);
                });
            }
            ra_salsa::EventKind::WillBlockOn { database_key, .. } => {
                with_query_name(self, database_key, |name| {
                    self.stats.record_blocked_on_other_runtime(name);
                });
            }
            ra_salsa::EventKind::WillCheckCancellation => {
                self.stats.record_cancel_check();
            }
        }

        // Coarse-grained instrumentation hook: the salsa macros already emit
        // `tracing::trace_span!` for memoized queries; this is primarily useful
        // for debugging cache behavior.
        #[cfg(feature = "tracing")]
        match event.kind {
            ra_salsa::EventKind::WillExecute { database_key } => {
                tracing::trace!(event = "will_execute", key = ?database_key.debug(self));
            }
            ra_salsa::EventKind::DidValidateMemoizedValue { database_key } => {
                tracing::trace!(event = "did_validate_memoized", key = ?database_key.debug(self));
            }
            ra_salsa::EventKind::WillBlockOn {
                other_runtime_id,
                database_key,
            } => {
                tracing::trace!(
                    event = "will_block_on",
                    other_runtime_id = ?other_runtime_id,
                    key = ?database_key.debug(self)
                );
            }
            ra_salsa::EventKind::WillCheckCancellation => {
                tracing::trace!(event = "will_check_cancellation");
            }
        }

        #[cfg(not(feature = "tracing"))]
        {
            let _ = event;
        }
    }
}

impl ra_salsa::ParallelDatabase for RootDatabase {
    fn snapshot(&self) -> ra_salsa::Snapshot<RootDatabase> {
        ra_salsa::Snapshot::new(RootDatabase {
            storage: self.storage.snapshot(),
            stats: self.stats.clone(),
            persistence: self.persistence.clone(),
            file_paths: self.file_paths.clone(),
            item_tree_store: self.item_tree_store.clone(),
            class_interner: self.class_interner.clone(),
            syntax_tree_store: self.syntax_tree_store.clone(),
            memo_footprint: self.memo_footprint.clone(),
            java_parse_cache: self.java_parse_cache.clone(),
            java_parse_store: self.java_parse_store.clone(),
        })
    }
}

/// Memory manager evictor for Salsa memoized query results.
pub struct SalsaMemoEvictor {
    name: String,
    db: Arc<ParkingMutex<RootDatabase>>,
    inputs: Arc<ParkingMutex<SalsaInputs>>,
    footprint: Arc<SalsaMemoFootprint>,
    registration: OnceLock<nova_memory::MemoryRegistration>,
}

impl SalsaMemoEvictor {
    fn new(
        db: Arc<ParkingMutex<RootDatabase>>,
        inputs: Arc<ParkingMutex<SalsaInputs>>,
        footprint: Arc<SalsaMemoFootprint>,
    ) -> Self {
        Self {
            name: "salsa_memos".to_string(),
            db,
            inputs,
            footprint,
            registration: OnceLock::new(),
        }
    }

    fn register(self: &Arc<Self>, manager: &MemoryManager) {
        if self.registration.get().is_some() {
            return;
        }

        let registration =
            manager.register_evictor(self.name.clone(), MemoryCategory::QueryCache, self.clone());
        self.footprint.bind_tracker(registration.tracker());
        let _ = self.registration.set(registration);
    }
}

impl MemoryEvictor for SalsaMemoEvictor {
    fn name(&self) -> &str {
        &self.name
    }

    fn category(&self) -> MemoryCategory {
        MemoryCategory::QueryCache
    }

    fn flush_to_disk(&self) -> std::io::Result<()> {
        // Persistence should be strictly best-effort under memory pressure:
        // - never panic
        // - never block eviction on cache write failures
        //
        // Avoid even attempting persistence when writes are disabled or the
        // cache directory is unavailable.
        let can_write = {
            let db = self.db.lock();
            db.persistence.mode().allows_write() && db.persistence.cache_dir().is_some()
        };
        if !can_write {
            return Ok(());
        }

        // Persist only "known" projects: projects that have `project_files`
        // set in the tracked inputs.
        let mut projects: Vec<ProjectId> = {
            let inputs = self.inputs.lock();
            inputs.project_files.keys().copied().collect()
        };
        projects.sort();
        projects.dedup();

        if projects.is_empty() {
            return Ok(());
        }

        // Reuse the existing `Database::persist_project_indexes` helper by
        // constructing a temporary wrapper around the shared db/input state.
        //
        // NOTE: This is only used for best-effort persistence under memory pressure, so
        // we initialize input memory trackers with fresh (unregistered) instances.
        let db = Database {
            inner: self.db.clone(),
            inputs: self.inputs.clone(),
            memo_evictor: Arc::new(OnceLock::new()),
            cancellation_on_memory_pressure: Arc::new(OnceLock::new()),
            memo_footprint: self.footprint.clone(),
            input_footprint: Arc::new(SalsaInputFootprint::default()),
            jdk_index_tracker: Arc::new(InputIndexTracker::new("jdk_index")),
            classpath_index_tracker: Arc::new(InputIndexTracker::new("classpath_index")),
        };

        for project in projects {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = db.persist_project_indexes(project);
            }));
        }

        Ok(())
    }

    fn evict(&self, request: EvictionRequest) -> EvictionResult {
        let before = self.footprint.bytes();
        if before <= request.target_bytes {
            return EvictionResult {
                before_bytes: before,
                after_bytes: before,
            };
        }

        // Eviction must be best-effort and non-panicking.
        //
        // NOTE(perf): As of `ra_salsa`/`ra_ap_salsa` 0.0.269 we did not find a
        // *safe* public API to drop memoized values for a particular query key
        // (e.g. "evict memos for `FileId(123)`"):
        //
        // - `QueryTableMut::invalidate(&key)` exists, but it only forces
        //   recomputation; it does **not** drop the stored value and therefore
        //   does not meaningfully reduce memory usage.
        // - `QueryTable::purge()` exists, but its docs explicitly warn that it
        //   breaks Salsa invariants ("any further queries might return nonsense
        //   results"), so it is unsuitable for production eviction.
        // - There is internal LRU-backed memo storage
        //   (`ra_salsa::plumbing::LruMemoizedStorage`) that can clear stored
        //   values, but it is query-definition opt-in and still does not expose
        //   a per-key/manual eviction hook.
        //
        // Until `ra_salsa` grows a stable "drop memo for key" / sweep API, the
        // least-worst option is to rebuild the database from inputs and swap it
        // behind the mutex. Outstanding snapshots remain valid because they own
        // their storage snapshots.
        //
        // Rebuilding would ordinarily also drop `#[ra_salsa::interned]` tables.
        // To keep interned IDs stable we snapshot+restore the relevant interned
        // entries (see `InternedTablesSnapshot`).
        let mut swapped = false;
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // Avoid cloning potentially large input maps during eviction (file
            // contents, per-project metadata, etc). Instead, hold the inputs
            // lock while applying them to the fresh database.
            //
            // Lock ordering: `inputs` then `db` (matches the rest of this file).
            let inputs = self.inputs.lock();
            let mut db = self.db.lock();
            let stats = db.stats.clone();
            let persistence = db.persistence.clone();
            let file_paths = db.file_paths.clone();
            let interned = InternedTablesSnapshot::capture(&db);
            let item_tree_store = db.item_tree_store.clone();
            let class_interner = db.class_interner.clone();
            let syntax_tree_store = db.syntax_tree_store.clone();
            let java_parse_cache = db.java_parse_cache.clone();
            java_parse_cache.clear();
            let java_parse_store = db.java_parse_store.clone();
            let mut fresh = RootDatabase {
                storage: ra_salsa::Storage::default(),
                stats,
                persistence,
                file_paths,
                item_tree_store,
                class_interner,
                syntax_tree_store,
                memo_footprint: self.footprint.clone(),
                java_parse_cache,
                java_parse_store,
            };
            inputs.apply_to(&mut fresh);
            if !interned.restore_into(&fresh) {
                return;
            }
            *db = fresh;
            swapped = true;
        }));

        if swapped {
            // Clear tracked footprint; memos will be re-recorded as queries
            // re-execute.
            self.footprint.clear();
        }
        let after = self.footprint.bytes();

        EvictionResult {
            before_bytes: before,
            after_bytes: after,
        }
    }
}

/// Thread-safe handle around [`RootDatabase`].
///
/// - Writes are serialized through an internal mutex.
/// - Reads are expected to happen through snapshots (`Database::snapshot`),
///   which can then be freely sent to worker threads.
#[derive(Clone)]
pub struct Database {
    inner: Arc<ParkingMutex<RootDatabase>>,
    inputs: Arc<ParkingMutex<SalsaInputs>>,
    memo_evictor: Arc<OnceLock<Arc<SalsaMemoEvictor>>>,
    cancellation_on_memory_pressure: Arc<OnceLock<()>>,
    memo_footprint: Arc<SalsaMemoFootprint>,
    input_footprint: Arc<SalsaInputFootprint>,
    jdk_index_tracker: Arc<InputIndexTracker>,
    classpath_index_tracker: Arc<InputIndexTracker>,
}

impl Default for Database {
    fn default() -> Self {
        let db = RootDatabase::default();
        let memo_footprint = db.memo_footprint.clone();
        let input_footprint = Arc::new(SalsaInputFootprint::default());
        let mut inputs = SalsaInputs::default();
        let default_project = ProjectId::from_raw(0);
        inputs
            .project_config
            .insert(default_project, db.project_config(default_project));
        inputs
            .project_class_ids
            .insert(default_project, db.project_class_ids(default_project));
        let jdk_index_tracker = Arc::new(InputIndexTracker::new("jdk_index"));
        let classpath_index_tracker = Arc::new(InputIndexTracker::new("classpath_index"));
        Self {
            inner: Arc::new(ParkingMutex::new(db)),
            inputs: Arc::new(ParkingMutex::new(inputs)),
            memo_evictor: Arc::new(OnceLock::new()),
            cancellation_on_memory_pressure: Arc::new(OnceLock::new()),
            memo_footprint,
            input_footprint,
            jdk_index_tracker,
            classpath_index_tracker,
        }
    }
}

impl Database {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn new_with_memory_manager(manager: &MemoryManager) -> Self {
        let db = Self::new();
        db.register_salsa_memo_evictor(manager);
        db.register_salsa_cancellation_on_memory_pressure(manager);
        db
    }

    /// Attach a shared [`nova_syntax::SyntaxTreeStore`] used to pin syntax trees
    /// for open documents.
    ///
    /// This is intentionally not a Salsa-tracked input: the presence/absence of
    /// the store must not change query results (only caching/pinning behavior).
    pub fn set_syntax_tree_store(&self, store: Arc<nova_syntax::SyntaxTreeStore>) {
        // When a pinned parse result is removed from the store (e.g. due to
        // memory pressure eviction), restore its memo footprint to avoid
        // undercounting the memory still held by Salsa memo tables.
        let memo_footprint = Arc::clone(&self.memo_footprint);
        store.set_on_remove(Arc::new(move |file: FileId, bytes: u64| {
            let should_restore = memo_footprint
                .lock_inner()
                .by_file
                .get(&file)
                .and_then(|bytes| bytes.parse)
                .is_some_and(|bytes| bytes == 0);
            if !should_restore {
                return;
            }
            memo_footprint.record(file, TrackedSalsaMemo::Parse, bytes);
        }));
        self.inner.lock().syntax_tree_store = Some(store);
    }

    /// Remove any pinned parse tree for `file` from the shared syntax tree store
    /// (if configured) and restore the parse entry in the Salsa memo footprint.
    ///
    /// This is intended to be called when an editor document is closed: the
    /// parse tree is no longer pinned and should once again be attributed to
    /// Salsa memoization for accounting purposes.
    pub fn unpin_syntax_tree(&self, file: FileId) {
        let store = self.inner.lock().syntax_tree_store.clone();
        if let Some(store) = store.as_ref() {
            store.remove(file);
        }

        // Only restore memo accounting if we have previously recorded a `parse`
        // memo for this file and suppressed it to `0` while pinned.
        let should_restore = self
            .memo_footprint
            .lock_inner()
            .by_file
            .get(&file)
            .and_then(|bytes| bytes.parse)
            .is_some_and(|bytes| bytes == 0);
        if !should_restore {
            return;
        }

        // Best-effort: restore the parse memo approximation based on the most
        // recent input text snapshot.
        let bytes = self
            .inputs
            .lock()
            .file_content
            .get(&file)
            .map(|text| text.len() as u64)
            .unwrap_or(0);
        self.memo_footprint
            .record(file, TrackedSalsaMemo::Parse, bytes);
    }

    /// Remove any pinned `item_tree` result for `file` from the shared
    /// [`ItemTreeStore`] (if configured) and restore the query-cache entry in
    /// the Salsa memo footprint.
    pub fn unpin_item_tree(&self, file: FileId) {
        let store = self.inner.lock().item_tree_store.clone();
        if let Some(store) = store.as_ref() {
            store.remove(file);
        }

        // Only restore memo accounting if we have previously recorded an
        // `item_tree` memo for this file and suppressed it to `0` while pinned.
        let should_restore = self
            .memo_footprint
            .lock_inner()
            .by_file
            .get(&file)
            .and_then(|bytes| bytes.item_tree)
            .is_some_and(|bytes| bytes == 0);
        if !should_restore {
            return;
        }

        // Best-effort: restore the item_tree memo approximation based on the
        // most recent input text snapshot.
        let bytes = self
            .inputs
            .lock()
            .file_content
            .get(&file)
            .map(|text| text.len() as u64)
            .unwrap_or(0);
        self.memo_footprint
            .record(file, TrackedSalsaMemo::ItemTree, bytes);
    }

    /// Remove any pinned `parse_java` result for `file` from the shared
    /// [`nova_syntax::JavaParseStore`] (if configured) and restore the memo
    /// footprint entry if it had been suppressed while pinned.
    ///
    /// This is intended to be called when an editor document is closed (or when
    /// a rename/move closes a `FileId`): the parse result is no longer pinned
    /// and should not be retained in the open-document store.
    pub fn unpin_java_parse_tree(&self, file: FileId) {
        let store = self.inner.lock().java_parse_store.clone();
        if let Some(store) = store.as_ref() {
            store.remove(file);
        }

        // Only restore memo accounting if we have previously recorded a
        // `parse_java` memo for this file and suppressed it to `0` while pinned.
        let should_restore = self
            .memo_footprint
            .lock_inner()
            .by_file
            .get(&file)
            .and_then(|bytes| bytes.parse_java)
            .is_some_and(|bytes| bytes == 0);
        if !should_restore {
            return;
        }

        // Best-effort: restore the parse_java memo approximation based on the most
        // recent input text snapshot.
        let bytes = self
            .inputs
            .lock()
            .file_content
            .get(&file)
            .map(|text| text.len() as u64)
            .unwrap_or(0);
        self.memo_footprint
            .record(file, TrackedSalsaMemo::ParseJava, bytes);
    }

    /// Remove any pinned `parse_java` result for `file` from the shared
    /// [`nova_syntax::JavaParseStore`] (if configured) and restore the query-cache
    /// entry in the Salsa memo footprint.
    pub fn unpin_java_parse(&self, file: FileId) {
        let store = self.inner.lock().java_parse_store.clone();
        if let Some(store) = store.as_ref() {
            store.remove(file);
        }

        // Only restore memo accounting if we have previously recorded a
        // `parse_java` memo for this file and suppressed it to `0` while pinned.
        let should_restore = self
            .memo_footprint
            .lock_inner()
            .by_file
            .get(&file)
            .and_then(|bytes| bytes.parse_java)
            .is_some_and(|bytes| bytes == 0);
        if !should_restore {
            return;
        }

        // Best-effort: restore the parse_java memo approximation based on the
        // most recent input text snapshot.
        let bytes = self
            .inputs
            .lock()
            .file_content
            .get(&file)
            .map(|text| text.len() as u64)
            .unwrap_or(0);
        self.memo_footprint
            .record(file, TrackedSalsaMemo::ParseJava, bytes);
    }

    pub fn new_with_persistence(
        project_root: impl AsRef<Path>,
        persistence: PersistenceConfig,
    ) -> Self {
        let db = RootDatabase::new_with_persistence(project_root, persistence);
        let memo_footprint = db.memo_footprint.clone();
        let input_footprint = Arc::new(SalsaInputFootprint::default());
        let mut inputs = SalsaInputs::default();
        let default_project = ProjectId::from_raw(0);
        inputs
            .project_config
            .insert(default_project, db.project_config(default_project));
        inputs
            .project_class_ids
            .insert(default_project, db.project_class_ids(default_project));
        let jdk_index_tracker = Arc::new(InputIndexTracker::new("jdk_index"));
        let classpath_index_tracker = Arc::new(InputIndexTracker::new("classpath_index"));
        Self {
            inner: Arc::new(ParkingMutex::new(db)),
            inputs: Arc::new(ParkingMutex::new(inputs)),
            memo_evictor: Arc::new(OnceLock::new()),
            cancellation_on_memory_pressure: Arc::new(OnceLock::new()),
            memo_footprint,
            input_footprint,
            jdk_index_tracker,
            classpath_index_tracker,
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        let all_file_ids = {
            let mut inputs = self.inputs.lock();
            if !inputs.file_ids_dirty {
                None
            } else {
                inputs.file_ids_dirty = false;
                Some(Arc::new(
                    inputs.file_ids.iter().copied().collect::<Vec<_>>(),
                ))
            }
        };

        let mut db = self.inner.lock();
        if let Some(all_file_ids) = all_file_ids {
            let bytes = (all_file_ids.len() as u64) * (std::mem::size_of::<FileId>() as u64);
            self.input_footprint.record_all_file_ids_bytes(bytes);
            db.set_all_file_ids(all_file_ids);
        }
        db.snapshot()
    }

    pub fn with_snapshot<T>(&self, f: impl FnOnce(&Snapshot) -> T) -> T {
        let snap = self.snapshot();
        f(&snap)
    }

    pub fn with_snapshot_catch_cancelled<T>(
        &self,
        f: impl FnOnce(&Snapshot) -> T,
    ) -> Result<T, ra_salsa::Cancelled> {
        let snap = self.snapshot();
        catch_cancelled(|| f(&snap))
    }

    pub fn query_stats(&self) -> QueryStats {
        self.inner.lock().query_stats()
    }

    pub fn clear_query_stats(&self) {
        self.inner.lock().clear_query_stats();
    }

    pub fn persistence_stats(&self) -> crate::PersistenceStats {
        self.inner.lock().persistence_stats()
    }

    /// Returns the per-project classpath cache directory (`<cache>/<project-hash>/classpath/`)
    /// when persistence is enabled.
    ///
    /// This is intended for consumers that build a [`nova_classpath::ClasspathIndex`] outside of
    /// the Salsa query graph (e.g. `nova-workspace` during project reload) and still want to reuse
    /// Nova's on-disk caches.
    pub fn classpath_cache_dir(&self) -> Option<PathBuf> {
        self.inner
            .lock()
            .persistence
            .cache_dir()
            .map(|dir| dir.classpath_dir())
    }

    pub fn with_write<T>(&self, f: impl FnOnce(&mut RootDatabase) -> T) -> T {
        let mut db = self.inner.lock();
        f(&mut db)
    }

    pub fn set_java_parse_store(&self, store: Option<Arc<nova_syntax::JavaParseStore>>) {
        if let Some(store) = store.as_ref() {
            // When a pinned parse_java result is removed from the store (e.g. due
            // to memory pressure eviction), restore its memo footprint to avoid
            // undercounting the memory still held by Salsa memo tables.
            let memo_footprint = Arc::clone(&self.memo_footprint);
            store.set_on_remove(Arc::new(move |file: FileId, bytes: u64| {
                let should_restore = memo_footprint
                    .lock_inner()
                    .by_file
                    .get(&file)
                    .and_then(|bytes| bytes.parse_java)
                    .is_some_and(|bytes| bytes == 0);
                if !should_restore {
                    return;
                }
                memo_footprint.record(file, TrackedSalsaMemo::ParseJava, bytes);
            }));
        }
        self.inner.lock().set_java_parse_store(store);
    }

    pub fn request_cancellation(&self) {
        self.inner.lock().request_cancellation();
    }

    pub fn set_file_exists(&self, file: FileId, exists: bool) {
        use std::collections::hash_map::Entry;

        let init_dirty = {
            let mut inputs = self.inputs.lock();
            inputs.file_exists.insert(file, exists);
            match inputs.file_is_dirty.entry(file) {
                Entry::Vacant(entry) => {
                    entry.insert(false);
                    true
                }
                Entry::Occupied(_) => false,
            }
        };

        let mut db = self.inner.lock();
        if init_dirty {
            db.set_file_is_dirty(file, false);
        }
        db.set_file_exists(file, exists);
    }

    pub fn set_file_is_dirty(&self, file: FileId, dirty: bool) {
        let mut inputs = self.inputs.lock();
        inputs.file_is_dirty.insert(file, dirty);
        drop(inputs);

        self.inner.lock().set_file_is_dirty(file, dirty);
    }

    pub fn set_file_content(&self, file: FileId, content: Arc<String>) {
        use std::collections::hash_map::Entry;

        self.input_footprint
            .record_file_text(file, &content, &content, None);

        let init_dirty = {
            let mut inputs = self.inputs.lock();
            inputs.file_content.insert(file, content.clone());
            inputs.file_prev_content.insert(file, content.clone());
            inputs.file_last_edit.insert(file, None);
            if inputs.file_ids.insert(file) {
                inputs.file_ids_dirty = true;
            }
            match inputs.file_is_dirty.entry(file) {
                Entry::Vacant(entry) => {
                    entry.insert(false);
                    true
                }
                Entry::Occupied(_) => false,
            }
        };

        let mut db = self.inner.lock();
        if init_dirty {
            db.set_file_is_dirty(file, false);
        }
        db.set_file_content(file, content.clone());
        db.set_file_prev_content(file, content);
        db.set_file_last_edit(file, None);
    }

    pub fn set_file_text(&self, file: FileId, text: impl Into<String>) {
        use std::collections::hash_map::Entry;

        let text = Arc::new(text.into());
        self.input_footprint
            .record_file_text(file, &text, &text, None);
        let default_project = ProjectId::from_raw(0);
        let default_root = SourceRootId::from_raw(0);
        let (
            set_default_project,
            set_default_root,
            set_default_rel_path,
            rel_path,
            project_files_update,
            set_default_classpath_index,
            project,
            init_dirty,
        ) = {
            let mut inputs = self.inputs.lock();
            inputs.file_exists.insert(file, true);
            inputs.file_content.insert(file, text.clone());
            inputs.file_prev_content.insert(file, text.clone());
            inputs.file_last_edit.insert(file, None);
            if inputs.file_ids.insert(file) {
                inputs.file_ids_dirty = true;
            }

            let set_default_project = !inputs.file_project.contains_key(&file);
            if set_default_project {
                inputs.file_project.insert(file, default_project);
            }

            let set_default_root = !inputs.source_root.contains_key(&file);
            if set_default_root {
                inputs.source_root.insert(file, default_root);
            }

            let (set_default_rel_path, rel_path) =
                if let Some(path) = inputs.file_rel_path.get(&file) {
                    (false, path.clone())
                } else {
                    let path = Arc::new(format!("file-{}.java", file.to_raw()));
                    inputs.file_rel_path.insert(file, path.clone());
                    (true, path)
                };

            let project = *inputs.file_project.get(&file).unwrap_or(&default_project);

            // Provide a minimal `project_files` input so workspace-wide queries can run
            // in single-file / test scenarios where the host hasn't populated a full
            // workspace model. Keep deterministic ordering by sorting by `file_rel_path`.
            let mut project_files_update: Option<Arc<Vec<FileId>>> = None;
            match inputs.project_files.get(&project) {
                Some(existing) if existing.as_ref().contains(&file) => {}
                Some(existing) => {
                    let mut files = existing.as_ref().clone();
                    files.push(file);
                    files.sort_by_key(|file| {
                        inputs
                            .file_rel_path
                            .get(file)
                            .map(|p| p.as_ref().clone())
                            .unwrap_or_else(|| format!("file-{}.java", file.to_raw()))
                    });
                    files.dedup();
                    let files = Arc::new(files);
                    inputs.project_files.insert(project, files.clone());
                    project_files_update = Some(files);
                }
                None => {
                    let files = Arc::new(vec![file]);
                    inputs.project_files.insert(project, files.clone());
                    project_files_update = Some(files);
                }
            }

            let set_default_classpath_index = !inputs.classpath_index.contains_key(&project);
            if set_default_classpath_index {
                // Optional input used by name resolution; default to `None` so
                // resolve/import queries can be used without requiring explicit
                // classpath setup.
                inputs.classpath_index.insert(project, None);
            }

            let init_dirty = match inputs.file_is_dirty.entry(file) {
                Entry::Vacant(entry) => {
                    entry.insert(false);
                    true
                }
                Entry::Occupied(_) => false,
            };

            (
                set_default_project,
                set_default_root,
                set_default_rel_path,
                rel_path,
                project_files_update.map(|files| (project, files)),
                set_default_classpath_index,
                project,
                init_dirty,
            )
        };

        // Keep Salsa input memory tracking in sync for implicit defaults.
        self.input_footprint
            .record_file_rel_path_len(file, rel_path.len() as u64);
        if let Some((project, files)) = project_files_update.as_ref() {
            let bytes = (files.len() as u64) * (std::mem::size_of::<FileId>() as u64);
            self.input_footprint
                .record_project_files_bytes(*project, bytes);
        }

        let mut db = self.inner.lock();
        if init_dirty {
            db.set_file_is_dirty(file, false);
        }
        db.set_file_exists(file, true);
        if set_default_project {
            db.set_file_project(file, default_project);
        }
        if set_default_root {
            db.set_source_root(file, default_root);
        }
        if set_default_rel_path {
            db.set_file_rel_path(file, rel_path);
        }
        if let Some((project, files)) = project_files_update {
            db.set_project_files(project, files);
        }
        if set_default_classpath_index {
            db.set_classpath_index(project, None);
        }
        db.set_file_content(file, text.clone());
        db.set_file_prev_content(file, text);
        db.set_file_last_edit(file, None);
        drop(db);

        // Keep index tracking in sync for implicit defaults.
        if set_default_classpath_index {
            self.classpath_index_tracker
                .set_project_ptr(project, None, 0);
        }
    }

    /// Apply a single byte-offset-based text edit to `file` and set its new contents.
    ///
    /// Callers provide both:
    /// - the edit range/replacement (`edit`), used for validation and incremental parsing metadata
    /// - the full post-edit text snapshot (`new_text`), which becomes the new `file_content` input
    ///
    /// This is primarily intended for LSP-style incremental document updates where the host
    /// already computed the updated text. Using the provided `new_text` avoids reconstructing the
    /// full file contents inside the database.
    ///
    /// The edit is stored in the `file_prev_content` / `file_last_edit` inputs so `parse_java`
    /// can attempt incremental reparsing with `nova_syntax::reparse_java`.
    pub fn apply_file_text_edit(
        &self,
        file: FileId,
        edit: nova_core::TextEdit,
        new_text: Arc<String>,
    ) {
        let default_project = ProjectId::from_raw(0);
        let default_root = SourceRootId::from_raw(0);

        let (old_text, syntax_edit, set_default_project, set_default_root, project_files_update) = {
            let mut inputs = self.inputs.lock();

            inputs.file_exists.insert(file, true);
            if inputs.file_ids.insert(file) {
                inputs.file_ids_dirty = true;
            }

            let set_default_project = !inputs.file_project.contains_key(&file);
            if set_default_project {
                inputs.file_project.insert(file, default_project);
            }

            let set_default_root = !inputs.source_root.contains_key(&file);
            if set_default_root {
                inputs.source_root.insert(file, default_root);
            }

            let old_text = inputs
                .file_content
                .get(&file)
                .cloned()
                .unwrap_or_else(|| Arc::new(String::new()));

            let start = u32::from(edit.range.start()) as usize;
            let end = u32::from(edit.range.end()) as usize;
            assert!(
                start <= end && end <= old_text.len(),
                "apply_file_text_edit: range out of bounds (start={start}, end={end}, len={})",
                old_text.len()
            );
            assert!(
                old_text.is_char_boundary(start) && old_text.is_char_boundary(end),
                "apply_file_text_edit: edit range is not on UTF-8 character boundaries (start={start}, end={end})"
            );

            #[cfg(debug_assertions)]
            {
                let mut expected = old_text.as_str().to_string();
                expected.replace_range(start..end, &edit.replacement);
                debug_assert_eq!(
                    expected.as_str(),
                    new_text.as_str(),
                    "apply_file_text_edit: new_text did not match applying edit to current contents"
                );
            }

            let syntax_edit: TextEdit = (&edit)
                .try_into()
                .expect("failed to convert edit into nova_syntax::TextEdit");

            // Store incremental parsing metadata so `parse_java` can attempt `reparse_java`.
            inputs.file_prev_content.insert(file, old_text.clone());
            inputs.file_content.insert(file, new_text.clone());
            inputs
                .file_last_edit
                .insert(file, Some(syntax_edit.clone()));
            inputs.file_is_dirty.insert(file, true);

            // Ensure `project_files(project)` includes the edited file.
            let project = inputs
                .file_project
                .get(&file)
                .copied()
                .unwrap_or(default_project);
            let mut update: Option<Arc<Vec<FileId>>> = None;
            match inputs.project_files.get(&project) {
                Some(existing) if existing.as_ref().contains(&file) => {}
                Some(existing) => {
                    let mut files = existing.as_ref().clone();
                    files.push(file);
                    files.sort_by_key(|f| f.to_raw());
                    files.dedup();
                    let files = Arc::new(files);
                    inputs.project_files.insert(project, Arc::clone(&files));
                    update = Some(files);
                }
                None => {
                    let files = Arc::new(vec![file]);
                    inputs.project_files.insert(project, Arc::clone(&files));
                    update = Some(files);
                }
            }

            (
                old_text,
                syntax_edit,
                set_default_project,
                set_default_root,
                update.map(|v| (project, v)),
            )
        };

        self.input_footprint
            .record_file_text(file, &new_text, &old_text, Some(&syntax_edit));
        if let Some((project, files)) = project_files_update.as_ref() {
            let bytes = (files.len() as u64) * (std::mem::size_of::<FileId>() as u64);
            self.input_footprint
                .record_project_files_bytes(*project, bytes);
        }

        let mut db = self.inner.lock();
        db.set_file_is_dirty(file, true);
        db.set_file_exists(file, true);
        if set_default_project {
            db.set_file_project(file, default_project);
        }
        if set_default_root {
            db.set_source_root(file, default_root);
        }
        if let Some((project, files)) = project_files_update {
            db.set_project_files(project, files);
        }
        db.set_file_content(file, new_text);
        db.set_file_prev_content(file, old_text);
        db.set_file_last_edit(file, Some(syntax_edit));
    }

    pub fn set_file_path(&self, file: FileId, path: impl Into<String>) {
        self.inner.lock().set_file_path(file, path);
    }

    pub fn set_project_files(&self, project: ProjectId, files: Arc<Vec<FileId>>) {
        use std::collections::hash_map::Entry;

        let bytes = (files.len() as u64) * (std::mem::size_of::<FileId>() as u64);
        self.input_footprint
            .record_project_files_bytes(project, bytes);

        let mut init_dirty_files = Vec::new();
        {
            let mut inputs = self.inputs.lock();
            inputs.project_files.insert(project, files.clone());
            for &file in files.as_ref() {
                if let Entry::Vacant(entry) = inputs.file_is_dirty.entry(file) {
                    entry.insert(false);
                    init_dirty_files.push(file);
                }
            }
        }

        let mut db = self.inner.lock();
        for file in init_dirty_files {
            db.set_file_is_dirty(file, false);
        }
        db.set_project_files(project, files);
    }

    pub fn set_file_rel_path(&self, file: FileId, rel_path: Arc<String>) {
        use std::collections::hash_map::Entry;

        self.input_footprint
            .record_file_rel_path_len(file, rel_path.len() as u64);

        let init_dirty = {
            let mut inputs = self.inputs.lock();
            inputs.file_rel_path.insert(file, Arc::clone(&rel_path));
            match inputs.file_is_dirty.entry(file) {
                Entry::Vacant(entry) => {
                    entry.insert(false);
                    true
                }
                Entry::Occupied(_) => false,
            }
        };

        let mut db = self.inner.lock();
        if init_dirty {
            db.set_file_is_dirty(file, false);
        }
        db.set_file_rel_path(file, Arc::clone(&rel_path));
        // Keep the non-tracked file path map in sync so existing persistence
        // caches (AST artifacts, derived caches) can reuse the same keys.
        db.set_file_path_arc(file, rel_path.clone());
    }

    pub fn set_project_config(&self, project: ProjectId, config: Arc<ProjectConfig>) {
        let bytes = estimated_project_config_bytes(&config);
        self.input_footprint
            .record_project_config_bytes(project, bytes);

        self.inputs
            .lock()
            .project_config
            .insert(project, config.clone());
        self.inner.lock().set_project_config(project, config);
    }

    pub fn set_project_class_ids(
        &self,
        project: ProjectId,
        mapping: Arc<Vec<(Arc<str>, ClassId)>>,
    ) {
        // Best-effort sizing: count the bytes of each binary name plus the per-entry tuple size.
        //
        // NOTE: This intentionally does not attempt to account for all `Arc` header overhead or
        // HashMap metadata; it is only used to make large host-managed inputs visible to the
        // memory manager so eviction of *caches* can react to input-driven pressure.
        let bytes = {
            let mut total =
                (mapping.len() as u64) * (std::mem::size_of::<(Arc<str>, ClassId)>() as u64);
            for (name, _) in mapping.as_ref() {
                total = total.saturating_add(name.len() as u64);
            }
            total
        };
        self.input_footprint
            .record_project_class_ids_bytes(project, bytes);

        self.inputs
            .lock()
            .project_class_ids
            .insert(project, mapping.clone());
        self.inner.lock().set_project_class_ids(project, mapping);
    }

    pub fn set_file_project(&self, file: FileId, project: ProjectId) {
        use std::collections::hash_map::Entry;

        let init_dirty = {
            let mut inputs = self.inputs.lock();
            inputs.file_project.insert(file, project);
            match inputs.file_is_dirty.entry(file) {
                Entry::Vacant(entry) => {
                    entry.insert(false);
                    true
                }
                Entry::Occupied(_) => false,
            }
        };

        let mut db = self.inner.lock();
        if init_dirty {
            db.set_file_is_dirty(file, false);
        }
        db.set_file_project(file, project);
    }

    pub fn set_jdk_index(&self, project: ProjectId, index: Arc<nova_jdk::JdkIndex>) {
        let bytes = index.estimated_bytes();
        let ptr = Arc::as_ptr(&index) as usize;
        let index = ArcEq::new(index);
        self.inputs.lock().jdk_index.insert(project, index.clone());
        self.inner.lock().set_jdk_index(project, index);
        self.jdk_index_tracker
            .set_project_ptr(project, Some(ptr), bytes);
    }

    pub fn set_classpath_index(
        &self,
        project: ProjectId,
        index: Option<Arc<nova_classpath::ClasspathIndex>>,
    ) {
        let (ptr, bytes) = match &index {
            Some(index) => (Some(Arc::as_ptr(index) as usize), index.estimated_bytes()),
            None => (None, 0),
        };
        let index = index.map(ArcEq::new);
        self.inputs
            .lock()
            .classpath_index
            .insert(project, index.clone());
        self.inner.lock().set_classpath_index(project, index);
        self.classpath_index_tracker
            .set_project_ptr(project, ptr, bytes);
    }

    pub fn set_source_root(&self, file: FileId, root: SourceRootId) {
        use std::collections::hash_map::Entry;

        let init_dirty = {
            let mut inputs = self.inputs.lock();
            inputs.source_root.insert(file, root);
            match inputs.file_is_dirty.entry(file) {
                Entry::Vacant(entry) => {
                    entry.insert(false);
                    true
                }
                Entry::Occupied(_) => false,
            }
        };

        let mut db = self.inner.lock();
        if init_dirty {
            db.set_file_is_dirty(file, false);
        }
        db.set_source_root(file, root);
    }

    /// Best-effort drop of memoized Salsa query results.
    ///
    /// Input queries and interned IDs (for the subset captured by
    /// [`InternedTablesSnapshot`]) are preserved; any outstanding snapshots remain
    /// valid.
    pub fn evict_salsa_memos(&self, pressure: MemoryPressure) {
        // Under low pressure, avoid disrupting cache locality.
        if matches!(pressure, MemoryPressure::Low) {
            return;
        }

        // NB: This is necessarily coarse (see `SalsaMemoEvictor::evict` for
        // details). We rebuild the underlying Salsa database because `ra_salsa`
        // doesn't currently provide a production-safe per-key memo eviction API.
        let mut swapped = false;
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // Avoid cloning potentially large input maps during eviction (file
            // contents, per-project metadata, etc). Instead, hold the inputs
            // lock while applying them to the fresh database.
            //
            // Lock ordering: `inputs` then `db` (matches the rest of this file).
            let inputs = self.inputs.lock();
            let mut db = self.inner.lock();
            let stats = db.stats.clone();
            let persistence = db.persistence.clone();
            let file_paths = db.file_paths.clone();
            let interned = InternedTablesSnapshot::capture(&db);
            let item_tree_store = db.item_tree_store.clone();
            let class_interner = db.class_interner.clone();
            let syntax_tree_store = db.syntax_tree_store.clone();
            let java_parse_cache = db.java_parse_cache.clone();
            java_parse_cache.clear();
            let java_parse_store = db.java_parse_store.clone();
            let mut fresh = RootDatabase {
                storage: ra_salsa::Storage::default(),
                stats,
                persistence,
                file_paths,
                item_tree_store,
                class_interner,
                syntax_tree_store,
                memo_footprint: self.memo_footprint.clone(),
                java_parse_cache,
                java_parse_store,
            };
            inputs.apply_to(&mut fresh);
            if !interned.restore_into(&fresh) {
                return;
            }
            *db = fresh;
            swapped = true;
        }));
        if swapped {
            self.memo_footprint.clear();
        }
    }

    /// Attach an open-document aware [`ItemTreeStore`] to the database.
    ///
    /// The store is non-tracked Salsa state and is cloned into snapshots; this
    /// allows open documents to reuse expensive `item_tree` results across Salsa
    /// memo eviction.
    pub fn attach_item_tree_store(
        &self,
        manager: &MemoryManager,
        open_docs: Arc<OpenDocuments>,
    ) -> Arc<ItemTreeStore> {
        // Avoid holding the DB lock while registering with the memory manager.
        if let Some(existing) = self.inner.lock().item_tree_store.clone() {
            return existing;
        }

        let store = ItemTreeStore::new(manager, open_docs);
        // When a pinned item_tree result is removed from the store (e.g. due to
        // memory pressure eviction), restore its memo footprint to avoid
        // undercounting the memory still held by Salsa memo tables.
        let memo_footprint = Arc::clone(&self.memo_footprint);
        store.set_on_remove(Arc::new(move |file: FileId, bytes: u64| {
            let should_restore = memo_footprint
                .lock_inner()
                .by_file
                .get(&file)
                .and_then(|bytes| bytes.item_tree)
                .is_some_and(|bytes| bytes == 0);
            if !should_restore {
                return;
            }
            memo_footprint.record(file, TrackedSalsaMemo::ItemTree, bytes);
        }));
        self.inner.lock().item_tree_store = Some(store.clone());
        store
    }

    pub fn salsa_memo_bytes(&self) -> u64 {
        self.memo_footprint.bytes()
    }

    pub fn salsa_input_bytes(&self) -> u64 {
        self.input_footprint.bytes()
    }

    pub fn register_salsa_input_tracker(&self, manager: &MemoryManager) {
        self.input_footprint.register(manager);
    }

    pub fn register_salsa_memo_evictor(&self, manager: &MemoryManager) -> Arc<SalsaMemoEvictor> {
        // `register_salsa_memo_evictor` is the main entrypoint used by workspace
        // initialization, so also ensure Salsa input memory and large external
        // indexes are visible to the manager.
        self.register_salsa_input_tracker(manager);
        self.register_input_index_trackers(manager);
        self.register_java_parse_cache_evictor(manager);

        if let Some(existing) = self.memo_evictor.get() {
            existing.clone()
        } else {
            let evictor = Arc::new(SalsaMemoEvictor::new(
                self.inner.clone(),
                self.inputs.clone(),
                self.memo_footprint.clone(),
            ));
            evictor.register(manager);
            let _ = self.memo_evictor.set(evictor.clone());
            evictor
        }
    }

    /// Register memory trackers for large Salsa inputs (JDK + classpath indexes).
    pub fn register_input_index_trackers(&self, manager: &MemoryManager) {
        self.jdk_index_tracker.register_evictor(
            manager,
            Arc::new(JdkIndexEvictor::new(
                self.inputs.clone(),
                self.jdk_index_tracker.clone(),
            )),
        );
        self.classpath_index_tracker.register_evictor(
            manager,
            Arc::new(ClasspathIndexEvictor::new(
                self.inner.clone(),
                self.inputs.clone(),
                self.classpath_index_tracker.clone(),
            )),
        );
    }

    pub fn register_java_parse_cache_evictor(&self, manager: &MemoryManager) {
        let cache = self.inner.lock().java_parse_cache.clone();
        cache.register(manager);
    }

    /// Subscribe to memory pressure events and request Salsa cancellation when the
    /// process is under `High` or `Critical` pressure.
    ///
    /// This is best-effort and deliberately avoids deadlocking if memory
    /// enforcement is invoked while holding the database write lock: we only
    /// request cancellation if we can acquire the lock without blocking.
    pub fn register_salsa_cancellation_on_memory_pressure(&self, manager: &MemoryManager) {
        // Only subscribe once per database instance to avoid accumulating duplicate listeners.
        if self.cancellation_on_memory_pressure.set(()).is_err() {
            return;
        }

        let db = Arc::downgrade(&self.inner);
        let db_for_initial_check = db.clone();
        manager.subscribe(Arc::new(move |event: nova_memory::MemoryEvent| {
            // Request cancellation whenever we're under High/Critical pressure.
            if !matches!(
                event.pressure,
                MemoryPressure::High | MemoryPressure::Critical
            ) {
                return;
            }

            let Some(db) = db.upgrade() else {
                return;
            };

            // Avoid blocking in the listener: the current thread may already be holding
            // the DB lock (e.g. if enforcement is called while writing inputs), in
            // which case blocking would deadlock.
            if let Some(mut guard) = db.try_lock() {
                guard.request_cancellation();
            };
        }));

        // Best-effort: if we register this listener while the process is already under high
        // pressure, we may not see another MemoryEvent until pressure changes. Trigger a
        // cancellation request eagerly in that case.
        if matches!(
            manager.pressure(),
            MemoryPressure::High | MemoryPressure::Critical
        ) {
            if let Some(db) = db_for_initial_check.upgrade() {
                if let Some(mut guard) = db.try_lock() {
                    guard.request_cancellation();
                }
            }
        }
    }

    pub fn persist_project_indexes(
        &self,
        project: ProjectId,
    ) -> Result<(), nova_index::IndexPersistenceError> {
        let snap = self.snapshot();
        if !snap.persistence().mode().allows_write() {
            return Ok(());
        }

        let Some(cache_dir) = snap.persistence().cache_dir() else {
            return Ok(());
        };

        // Persisted indexes are validated primarily via on-disk metadata (mtime + size). If any
        // existing project file is currently "dirty" (contains in-memory edits not reflected on
        // disk), the in-memory indexes would not correspond to a stable on-disk snapshot and
        // could be reused incorrectly on the next warm-start. Treat persistence as a no-op in
        // that case.
        for &file in snap.project_files(project).iter() {
            if !snap.file_exists(file) {
                continue;
            }
            if snap.file_is_dirty(file) {
                return Ok(());
            }
        }

        use std::collections::{BTreeMap, BTreeSet};

        use nova_cache::{CacheMetadata, CacheMetadataArchive, Fingerprint, ProjectSnapshot};

        let shard_count = nova_index::DEFAULT_SHARD_COUNT;

        // Build a "fast" snapshot based on file metadata (mtime + size) so we can
        // determine which files likely changed without hashing every file.
        let mut path_to_file = BTreeMap::<String, FileId>::new();
        let mut stamp_map = BTreeMap::<String, Fingerprint>::new();
        for &file in snap.project_files(project).iter() {
            if !snap.file_exists(file) {
                continue;
            }

            let rel_path = snap.file_rel_path(file);
            let rel_path = rel_path.as_ref().clone();
            path_to_file.insert(rel_path.clone(), file);

            let full_path = cache_dir.project_root().join(&rel_path);
            let fp = Fingerprint::from_file_metadata(full_path)
                .unwrap_or_else(|_| snap.file_fingerprint(file).as_ref().clone());
            stamp_map.insert(rel_path, fp);
        }

        let stamp_snapshot = ProjectSnapshot::from_parts(
            cache_dir.project_root().to_path_buf(),
            cache_dir.project_hash().clone(),
            stamp_map,
        );

        let existing_paths: BTreeSet<String> = path_to_file.keys().cloned().collect();
        let all_existing_files: Vec<String> = existing_paths.iter().cloned().collect();

        let metadata_path = cache_dir.metadata_path();
        let metadata_exists = metadata_path.exists() || cache_dir.metadata_bin_path().exists();

        let mut invalidated_files = if metadata_exists {
            CacheMetadataArchive::open(&metadata_path)
                .ok()
                .flatten()
                .filter(|m| {
                    m.is_compatible() && m.project_hash() == cache_dir.project_hash().as_str()
                })
                .map(|m| m.diff_files_fast(&stamp_snapshot))
                .unwrap_or_else(|| all_existing_files.clone())
        } else {
            all_existing_files.clone()
        };

        let invalidated_existing: BTreeSet<String> = invalidated_files
            .iter()
            .filter(|path| existing_paths.contains(*path))
            .cloned()
            .collect();
        let indexing_all_files = invalidated_existing == existing_paths;

        let mut content_fingerprints: BTreeMap<String, Fingerprint> = if indexing_all_files {
            BTreeMap::new()
        } else if metadata_exists {
            CacheMetadata::load(&metadata_path)
                .ok()
                .filter(|m| m.is_compatible() && &m.project_hash == cache_dir.project_hash())
                .map(|m| m.file_fingerprints)
                .unwrap_or_default()
        } else {
            BTreeMap::new()
        };

        // Ensure every existing file has a content hash; missing entries must be hashed now.
        for path in &all_existing_files {
            if !content_fingerprints.contains_key(path) {
                invalidated_files.push(path.clone());
            }
        }
        invalidated_files.sort();
        invalidated_files.dedup();

        for path in &invalidated_files {
            let Some(&file) = path_to_file.get(path) else {
                continue;
            };

            let fp = snap.file_fingerprint(file);
            content_fingerprints.insert(path.clone(), fp.as_ref().clone());
        }

        // Drop fingerprints for deleted files.
        content_fingerprints.retain(|path, _| existing_paths.contains(path));

        let snapshot = ProjectSnapshot::from_parts(
            cache_dir.project_root().to_path_buf(),
            cache_dir.project_hash().clone(),
            content_fingerprints,
        );

        let shards = snap.project_index_shards(project);
        let mut shards = (*shards).clone();
        nova_index::save_sharded_indexes(cache_dir, &snapshot, shard_count, &mut shards)
    }
}

impl crate::SourceDatabase for Snapshot {
    fn file_content(&self, file_id: FileId) -> Arc<String> {
        let db: &RootDatabase = self;
        NovaInputs::file_content(db, file_id)
    }

    fn file_path(&self, file_id: FileId) -> Option<PathBuf> {
        self.file_paths
            .read()
            .get(&file_id)
            .map(|path| PathBuf::from(path.as_str()))
    }

    fn all_file_ids(&self) -> Arc<Vec<FileId>> {
        let db: &RootDatabase = self;
        NovaInputs::all_file_ids(db)
    }

    fn file_id(&self, path: &Path) -> Option<FileId> {
        let path = path.to_str()?;
        self.file_paths
            .read()
            .iter()
            .find_map(|(&file_id, stored)| (stored.as_str() == path).then_some(file_id))
    }
}

impl crate::SourceDatabase for Database {
    fn file_content(&self, file_id: FileId) -> Arc<String> {
        self.with_snapshot(|snap| crate::SourceDatabase::file_content(snap, file_id))
    }

    fn file_path(&self, file_id: FileId) -> Option<PathBuf> {
        self.with_snapshot(|snap| crate::SourceDatabase::file_path(snap, file_id))
    }

    fn all_file_ids(&self) -> Arc<Vec<FileId>> {
        self.with_snapshot(|snap| crate::SourceDatabase::all_file_ids(snap))
    }

    fn file_id(&self, path: &Path) -> Option<FileId> {
        let path = path.to_path_buf();
        self.with_snapshot(|snap| crate::SourceDatabase::file_id(snap, &path))
    }
}

impl ProjectDatabase for Database {
    fn project_files(&self) -> Vec<PathBuf> {
        let file_ids = crate::SourceDatabase::all_file_ids(self);
        let mut paths: Vec<PathBuf> = file_ids
            .as_ref()
            .iter()
            .filter_map(|file_id| crate::SourceDatabase::file_path(self, *file_id))
            .collect();
        paths.sort();
        paths.dedup();
        paths
    }

    fn file_text(&self, path: &Path) -> Option<String> {
        let file_id = crate::SourceDatabase::file_id(self, path)?;
        Some(
            crate::SourceDatabase::file_content(self, file_id)
                .as_ref()
                .clone(),
        )
    }
}

/// Convenience trait alias that composes Nova's query groups.
pub trait NovaDatabase:
    NovaInputs
    + NovaSyntax
    + NovaSemantic
    + NovaFlow
    + NovaHir
    + NovaResolve
    + NovaTypeck
    + NovaDiagnostics
    + NovaIde
    + NovaIndexing
{
}

impl<T> NovaDatabase for T where
    T: NovaInputs
        + NovaSyntax
        + NovaSemantic
        + NovaIde
        + NovaFlow
        + NovaHir
        + NovaResolve
        + NovaTypeck
        + NovaDiagnostics
        + NovaIndexing
{
}

#[cfg(test)]
fn assert_query_is_cancelled<T, F>(mut db: RootDatabase, run_query: F)
where
    T: Send + 'static,
    F: FnOnce(&Snapshot) -> T + Send + 'static,
{
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    const ENTER_TIMEOUT: Duration = Duration::from_secs(5);
    const CANCEL_TIMEOUT: Duration = Duration::from_secs(5);
    const HARNESS_TIMEOUT: Duration = Duration::from_secs(10);

    let harness = std::thread::spawn(move || -> Result<(), String> {
        let (entered_tx, entered_rx) = mpsc::channel();
        let snap = db.snapshot();

        let worker = std::thread::spawn(move || {
            let _guard =
                cancellation::test_support::install_entered_long_running_region_sender(entered_tx);
            catch_cancelled(|| run_query(&snap))
        });

        entered_rx.recv_timeout(ENTER_TIMEOUT).map_err(|_| {
            "query never hit a cancellation checkpoint (missing checkpoint_cancelled?)".to_string()
        })?;

        // NB: this may block until the query unwinds and drops its snapshot.
        db.request_cancellation();

        let deadline = Instant::now() + CANCEL_TIMEOUT;
        while !worker.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        if !worker.is_finished() {
            return Err(format!(
                "query did not unwind with ra_salsa::Cancelled within {CANCEL_TIMEOUT:?} after request_cancellation"
            ));
        }

        let result = worker
            .join()
            .map_err(|_| "worker thread panicked".to_string())?;
        if result.is_ok() {
            return Err(
                "expected salsa query to unwind with Cancelled after request_cancellation"
                    .to_string(),
            );
        }

        Ok(())
    });

    let deadline = Instant::now() + HARNESS_TIMEOUT;
    while !harness.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        harness.is_finished(),
        "cancellation harness did not complete within {HARNESS_TIMEOUT:?}"
    );

    match harness.join().expect("cancellation harness panicked") {
        Ok(()) => {}
        Err(message) => panic!("{message}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nova_cache::{CacheConfig, Fingerprint};
    use nova_hir::hir::{Body, Expr, ExprId};
    use nova_memory::{MemoryBudget, MemoryCategory, MemoryPressure, GB};
    use nova_syntax::SyntaxTreeStore;
    use nova_vfs::OpenDocuments;
    use std::collections::BTreeMap;
    use std::sync::atomic::Ordering;
    use tempfile::TempDir;

    fn stat(db: &RootDatabase, query_name: &str) -> QueryStat {
        db.query_stats()
            .by_query
            .get(query_name)
            .copied()
            .unwrap_or_default()
    }

    fn executions(db: &RootDatabase, query_name: &str) -> u64 {
        stat(db, query_name).executions
    }

    fn expr_path(body: &Body, expr: ExprId) -> Option<String> {
        match &body.exprs[expr] {
            Expr::Name { name, .. } => Some(name.clone()),
            Expr::FieldAccess { receiver, name, .. } => {
                let mut path = expr_path(body, *receiver)?;
                path.push('.');
                path.push_str(name);
                Some(path)
            }
            _ => None,
        }
    }

    fn assert_query_is_cancelled_by_memory_pressure<T, F>(
        manager: MemoryManager,
        db: Database,
        run_query: F,
    ) where
        T: Send + 'static,
        F: FnOnce(&Snapshot) -> T + Send + 'static,
    {
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        const ENTER_TIMEOUT: Duration = Duration::from_secs(5);
        const CANCEL_TIMEOUT: Duration = Duration::from_secs(5);
        const HARNESS_TIMEOUT: Duration = Duration::from_secs(10);

        let harness = std::thread::spawn(move || -> Result<(), String> {
            let (entered_tx, entered_rx) = mpsc::channel();
            let snap = db.snapshot();

            let worker = std::thread::spawn(move || {
                let _guard = cancellation::test_support::install_entered_long_running_region_sender(
                    entered_tx,
                );
                catch_cancelled(|| run_query(&snap))
            });

            entered_rx.recv_timeout(ENTER_TIMEOUT).map_err(|_| {
                "query never hit a cancellation checkpoint (missing checkpoint_cancelled?)"
                    .to_string()
            })?;

            // Synthesize memory pressure and drive an enforcement pass to emit a MemoryEvent.
            let budget = manager.budget();
            let registration = manager.register_tracker("pressure_test", MemoryCategory::Other);
            registration
                .tracker()
                .set_bytes(budget.total.saturating_mul(2));
            manager.enforce();

            let deadline = Instant::now() + CANCEL_TIMEOUT;
            while !worker.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            if !worker.is_finished() {
                return Err(format!(
                    "query did not unwind with ra_salsa::Cancelled within {CANCEL_TIMEOUT:?} after memory pressure event"
                ));
            }

            let result = worker
                .join()
                .map_err(|_| "worker thread panicked".to_string())?;
            if result.is_ok() {
                return Err(
                    "expected salsa query to unwind with Cancelled after memory pressure event"
                        .to_string(),
                );
            }

            Ok(())
        });

        let deadline = Instant::now() + HARNESS_TIMEOUT;
        while !harness.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            harness.is_finished(),
            "cancellation harness did not complete within {HARNESS_TIMEOUT:?}"
        );

        match harness.join().expect("cancellation harness panicked") {
            Ok(()) => {}
            Err(message) => panic!("{message}"),
        }
    }

    #[test]
    fn classpath_cache_dir_is_available_when_persistence_enabled() {
        let tmp = TempDir::new().unwrap();
        let project_root = tmp.path().join("project");
        std::fs::create_dir_all(&project_root).unwrap();

        let cache_root = tmp.path().join("cache-root");
        let config = PersistenceConfig {
            mode: crate::persistence::PersistenceMode::ReadWrite,
            cache: CacheConfig {
                cache_root_override: Some(cache_root),
            },
        };

        let db = Database::new_with_persistence(&project_root, config);
        let classpath_dir = db
            .classpath_cache_dir()
            .expect("expected classpath cache dir when persistence enabled");
        assert!(classpath_dir.is_dir());
        assert!(classpath_dir.ends_with("classpath"));
    }

    #[test]
    fn edit_invalidates_parse() {
        let mut db = RootDatabase::default();
        let file = FileId::from_raw(1);

        db.set_file_text(file, "class Foo {}");

        let first = db.parse(file);
        assert_eq!(executions(&db, "parse"), 1);

        // Add tokens so the parse tree changes (not just ranges).
        db.set_file_text(file, "class Foo { int x; }");
        let second = db.parse(file);

        assert_eq!(executions(&db, "parse"), 2);
        assert_ne!(&*first, &*second);
    }

    #[test]
    fn whitespace_edit_reparses_but_early_cutoff_downstream() {
        let mut db = RootDatabase::default();
        let file = FileId::from_raw(1);

        db.set_file_text(file, "class Foo {}");

        let first_count = db.symbol_count(file);
        assert_eq!(first_count, 1);

        assert_eq!(executions(&db, "parse"), 1);
        assert_eq!(executions(&db, "item_tree"), 1);
        assert_eq!(executions(&db, "symbol_summary"), 1);
        assert_eq!(executions(&db, "symbol_count"), 1);

        // Whitespace-only change at the *start* of the file shifts token ranges,
        // which forces `item_tree` + `symbol_summary` to recompute.
        //
        // However, `symbol_summary` is stable (it only contains names), so
        // `symbol_count` can be reused via early-cutoff.
        db.set_file_text(file, "  class Foo {}");
        let second_count = db.symbol_count(file);

        assert_eq!(second_count, first_count);
        assert_eq!(executions(&db, "parse"), 2);
        assert_eq!(executions(&db, "item_tree"), 2);
        assert_eq!(
            executions(&db, "symbol_summary"),
            2,
            "symbol summary must recompute because ItemTree is range-sensitive"
        );
        assert_eq!(
            executions(&db, "symbol_count"),
            1,
            "dependent query should be reused due to early-cutoff"
        );
    }

    #[test]
    fn hir_item_tree_contains_expected_member_names() {
        use nova_hir::item_tree::{Item as HirItem, Member as HirMember};

        let mut db = RootDatabase::default();
        let file = FileId::from_raw(1);

        db.set_file_text(file, "class Foo { int x; void bar() {} }");

        let tree = db.hir_item_tree(file);
        assert_eq!(tree.items.len(), 1);

        let class_id = match tree.items[0] {
            HirItem::Class(id) => id,
            other => panic!("expected top-level class, got {other:?}"),
        };
        let class = tree.class(class_id);
        assert_eq!(class.name, "Foo");

        let mut saw_field = false;
        let mut saw_method = false;
        for member in &class.members {
            match member {
                HirMember::Field(id) => {
                    saw_field = true;
                    assert_eq!(tree.field(*id).name, "x");
                }
                HirMember::Method(id) => {
                    saw_method = true;
                    assert_eq!(tree.method(*id).name, "bar");
                }
                _ => {}
            }
        }

        assert!(saw_field, "expected to find field `x` in class members");
        assert!(saw_method, "expected to find method `bar` in class members");
    }

    #[test]
    fn hir_body_edit_early_cutoff_preserves_structural_name_queries() {
        use nova_hir::item_tree::{Item as HirItem, Member as HirMember};

        let mut db = RootDatabase::default();
        let file = FileId::from_raw(1);

        db.set_file_text(file, "class Foo { int x; void bar() { int y = 1; } }");

        let first = db.hir_symbol_names(file);
        assert_eq!(
            &*first,
            &["Foo".to_string(), "x".to_string(), "bar".to_string()]
        );

        let tree_before = db.hir_item_tree(file);
        let class_id = match tree_before.items[0] {
            HirItem::Class(id) => id,
            other => panic!("expected top-level class, got {other:?}"),
        };
        let class = tree_before.class(class_id);
        let method = class
            .members
            .iter()
            .find_map(|member| match member {
                HirMember::Method(id) => Some(*id),
                _ => None,
            })
            .expect("expected to find method `bar` in class members");

        let body = tree_before.method(method).body.expect("method has a body");
        let range_before = db
            .hir_ast_id_map(file)
            .span(body)
            .expect("method body span should exist");

        assert_eq!(executions(&db, "java_parse"), 1);
        assert_eq!(executions(&db, "hir_item_tree"), 1);
        assert_eq!(executions(&db, "hir_symbol_names"), 1);

        db.set_file_text(
            file,
            "class Foo { int x; void bar() { int y = 1; int z = 0; } }",
        );
        let second = db.hir_symbol_names(file);

        assert_eq!(&*second, &*first);
        assert_eq!(executions(&db, "java_parse"), 2);
        assert_eq!(executions(&db, "hir_item_tree"), 2);

        let tree_after = db.hir_item_tree(file);
        let body_after = tree_after
            .method(method)
            .body
            .expect("method still has a body");
        let range_after = db
            .hir_ast_id_map(file)
            .span(body_after)
            .expect("method body span should exist");
        assert_eq!(range_after.start, range_before.start);
        assert!(
            range_after.end > range_before.end,
            "expected method body range to expand after adding a statement"
        );

        assert_eq!(
            executions(&db, "hir_symbol_names"),
            1,
            "dependent query should be reused due to early-cutoff"
        );
    }

    #[test]
    fn hir_whitespace_edit_early_cutoff_preserves_structural_name_queries() {
        use nova_hir::item_tree::{Item as HirItem, Member as HirMember};

        let mut db = RootDatabase::default();
        let file = FileId::from_raw(1);

        db.set_file_text(file, "class Foo { int x; void bar() {} }");

        let first = db.hir_symbol_names(file);
        assert_eq!(
            &*first,
            &["Foo".to_string(), "x".to_string(), "bar".to_string()]
        );

        let tree_before = db.hir_item_tree(file);
        let class_id = match tree_before.items[0] {
            HirItem::Class(id) => id,
            other => panic!("expected top-level class, got {other:?}"),
        };
        let class = tree_before.class(class_id);
        let method = class
            .members
            .iter()
            .find_map(|member| match member {
                HirMember::Method(id) => Some(*id),
                _ => None,
            })
            .expect("expected to find method `bar` in class members");

        let body = tree_before.method(method).body.expect("method has a body");
        let range_before = db
            .hir_ast_id_map(file)
            .span(body)
            .expect("method body span should exist");

        assert_eq!(executions(&db, "java_parse"), 1);
        assert_eq!(executions(&db, "hir_item_tree"), 1);
        assert_eq!(executions(&db, "hir_symbol_names"), 1);

        // Leading whitespace shifts spans throughout the file but should not force
        // recomputation of downstream name-only queries.
        db.set_file_text(file, "  class Foo { int x; void bar() {} }");
        let second = db.hir_symbol_names(file);

        assert_eq!(&*second, &*first);
        assert_eq!(executions(&db, "java_parse"), 2);
        assert_eq!(executions(&db, "hir_item_tree"), 2);

        let tree_after = db.hir_item_tree(file);
        let body_after = tree_after
            .method(method)
            .body
            .expect("method still has a body");
        let range_after = db
            .hir_ast_id_map(file)
            .span(body_after)
            .expect("method body span should exist");
        assert_eq!(
            range_after.start,
            range_before.start + 2,
            "expected leading whitespace to shift method body range start by two bytes"
        );

        assert_eq!(
            executions(&db, "hir_symbol_names"),
            1,
            "dependent query should be reused due to early-cutoff"
        );
    }

    #[test]
    fn hir_body_queries_lower_locals_and_calls() {
        use nova_hir::item_tree::{Item as HirItem, Member as HirMember};

        let mut db = RootDatabase::default();
        let file = FileId::from_raw(1);

        let source = r#"
package com.example;

import java.util.List;
import java.util.*;
import static java.lang.Math.*;
import static java.lang.Math.PI;

@interface Marker {
    int value() default 1;
}

class Foo {
    int field;

    static {
        final int s = 0;
        System.out.println(s);
    }

    Foo(final int a) {
        final int x = a;
        bar(x);
    }

    class Inner {}

    @interface InnerAnn {}

    void bar(final int y) {
        final int z = y + 1;
        System.out.println(z);
        return;
    }
}
"#;

        db.set_file_text(file, source);

        let tree = db.hir_item_tree(file);
        assert_eq!(
            tree.package.as_ref().map(|pkg| pkg.name.as_str()),
            Some("com.example")
        );

        let foo_id = tree
            .items
            .iter()
            .find_map(|item| match item {
                HirItem::Class(id) if tree.class(*id).name == "Foo" => Some(*id),
                _ => None,
            })
            .expect("expected Foo class");
        let foo = tree.class(foo_id);

        let inner_id = foo
            .members
            .iter()
            .find_map(|member| match member {
                HirMember::Type(HirItem::Class(id)) if tree.class(*id).name == "Inner" => Some(*id),
                _ => None,
            })
            .expect("expected nested Inner class");
        assert_eq!(tree.class(inner_id).name, "Inner");

        let bar_id = foo
            .members
            .iter()
            .find_map(|member| match member {
                HirMember::Method(id) if tree.method(*id).name == "bar" => Some(*id),
                _ => None,
            })
            .expect("bar method");
        let body = db.hir_body(bar_id);

        let local_names: Vec<_> = body
            .locals
            .iter()
            .map(|(_, local)| local.name.as_str())
            .collect();
        assert_eq!(local_names, vec!["z"]);

        let mut call_paths = Vec::new();
        for (id, expr) in body.exprs.iter() {
            if let Expr::Call { callee, .. } = expr {
                let callee_path =
                    expr_path(&body, *callee).unwrap_or_else(|| format!("ExprId({id})"));
                call_paths.push(callee_path);
            }
        }
        assert!(call_paths.iter().any(|path| path == "System.out.println"));

        let ctor_id = foo
            .members
            .iter()
            .find_map(|member| match member {
                HirMember::Constructor(id) => Some(*id),
                _ => None,
            })
            .expect("expected Foo constructor");
        let ctor_body = db.hir_constructor_body(ctor_id);
        let ctor_locals: Vec<_> = ctor_body
            .locals
            .iter()
            .map(|(_, local)| local.name.as_str())
            .collect();
        assert_eq!(ctor_locals, vec!["x"]);

        let mut ctor_call_paths = Vec::new();
        for (id, expr) in ctor_body.exprs.iter() {
            if let Expr::Call { callee, .. } = expr {
                let callee_path =
                    expr_path(&ctor_body, *callee).unwrap_or_else(|| format!("ExprId({id})"));
                ctor_call_paths.push(callee_path);
            }
        }
        assert!(ctor_call_paths.iter().any(|path| path == "bar"));

        let init_id = foo
            .members
            .iter()
            .find_map(|member| match member {
                HirMember::Initializer(id) if tree.initializer(*id).is_static => Some(*id),
                _ => None,
            })
            .expect("static initializer");
        let init_body = db.hir_initializer_body(init_id);
        let init_locals: Vec<_> = init_body
            .locals
            .iter()
            .map(|(_, local)| local.name.as_str())
            .collect();
        assert_eq!(init_locals, vec!["s"]);
    }

    #[test]
    fn snapshots_are_consistent_across_concurrent_reads() {
        let mut db = RootDatabase::default();
        let file = FileId::from_raw(1);

        db.set_file_text(file, "class Foo {}");

        let snap1 = db.snapshot();
        let snap2 = db.snapshot();

        let h1 = std::thread::spawn(move || snap1.symbol_summary(file).names.clone());
        let h2 = std::thread::spawn(move || snap2.symbol_summary(file).names.clone());

        let from_snap1 = h1.join().expect("snapshot 1 panicked");
        let from_snap2 = h2.join().expect("snapshot 2 panicked");

        assert_eq!(from_snap1, vec!["Foo".to_string()]);
        assert_eq!(from_snap1, from_snap2);
    }

    #[test]
    fn request_cancellation_unwinds_inflight_queries() {
        let mut db = RootDatabase::default();
        let file = FileId::from_raw(1);
        db.set_source_root(file, SourceRootId::from_raw(0));
        db.set_file_text(file, "class Foo {}");

        assert_query_is_cancelled(db, move |snap| snap.interruptible_work(file, 5_000_000));
    }

    #[test]
    fn memory_pressure_event_requests_salsa_cancellation() {
        let manager = MemoryManager::new(MemoryBudget::from_total(8 * GB));
        let db = Database::new();
        db.register_salsa_cancellation_on_memory_pressure(&manager);

        let file = FileId::from_raw(1);
        db.set_file_text(file, "class Foo {}");

        assert_query_is_cancelled_by_memory_pressure(manager, db, move |snap| {
            snap.interruptible_work(file, 5_000_000)
        });
    }

    #[test]
    fn registering_under_existing_high_pressure_requests_salsa_cancellation() {
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        const ENTER_TIMEOUT: Duration = Duration::from_secs(5);
        const CANCEL_TIMEOUT: Duration = Duration::from_secs(5);

        // Put the manager under critical pressure *before* registering the listener. We do not
        // call `enforce()`, so no MemoryEvent will be emitted unless the registration code checks
        // current pressure eagerly.
        let manager = MemoryManager::new(MemoryBudget::from_total(8 * GB));
        let registration = manager.register_tracker("pressure_test", MemoryCategory::Other);
        registration
            .tracker()
            .set_bytes(manager.budget().total.saturating_mul(2));

        let db = Database::new();
        let file = FileId::from_raw(1);
        db.set_file_text(file, "class Foo {}");

        let snap = db.snapshot();
        let (entered_tx, entered_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _guard =
                cancellation::test_support::install_entered_long_running_region_sender(entered_tx);
            catch_cancelled(|| snap.interruptible_work(file, 5_000_000))
        });

        entered_rx
            .recv_timeout(ENTER_TIMEOUT)
            .expect("interruptible_work did not reach a cancellation checkpoint");

        // Registering should request cancellation immediately since pressure is already critical.
        db.register_salsa_cancellation_on_memory_pressure(&manager);

        let deadline = Instant::now() + CANCEL_TIMEOUT;
        while !worker.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            worker.is_finished(),
            "query did not unwind within {CANCEL_TIMEOUT:?} after registering under high pressure"
        );

        let result = worker.join().expect("worker thread panicked");
        assert!(
            result.is_err(),
            "expected salsa query to unwind with Cancelled when registering under high pressure"
        );
    }

    #[test]
    fn request_cancellation_unwinds_synthetic_semantic_query() {
        let mut db = RootDatabase::default();
        let file = FileId::from_raw(1);
        db.set_source_root(file, SourceRootId::from_raw(0));
        db.set_file_text(file, "class Foo {}");

        assert_query_is_cancelled(db, move |snap| {
            snap.synthetic_semantic_work(file, 5_000_000)
        });
    }

    #[test]
    fn request_cancellation_unwinds_hir_body_query() {
        use std::fmt::Write as _;

        let mut db = RootDatabase::default();
        let file = FileId::from_raw(1);
        db.set_source_root(file, SourceRootId::from_raw(0));

        // Build a body large enough to exceed the HIR lowering cancellation checkpoint interval.
        let mut source = String::from("class Foo { void m() {");
        // Use a body large enough that the query runs long enough for the cancellation request
        // (issued from another thread) to reliably land while `hir_body` is still executing,
        // without making `parse_block` so slow that we miss the ENTER_TIMEOUT.
        for i in 0..8_000_u32 {
            let _ = write!(source, "int v{i} = {i};");
        }
        source.push_str("} }");
        db.set_file_text(file, source);

        // Prime `hir_item_tree` (and its dependencies) so the cancellation harness exercises the
        // method body lowering work in `hir_body`.
        let tree = db.hir_item_tree(file);
        let (&method_ast_id, _) = tree
            .methods
            .iter()
            .find(|(_, method)| method.name == "m")
            .expect("expected Foo.m method");
        let method_id = nova_hir::ids::MethodId::new(file, method_ast_id);

        assert_query_is_cancelled(db, move |snap| {
            let _ = snap.hir_body(method_id);
        });
    }

    #[test]
    fn request_cancellation_unwinds_flow_diagnostics_query() {
        let mut db = RootDatabase::default();
        let file = FileId::from_raw(1);
        db.set_source_root(file, SourceRootId::from_raw(0));

        // Build a body large enough that `flow_diagnostics` is still executing after the
        // cancellation request is issued.
        let mut source = String::from("class Foo { void m() { int x = 0;");
        // Use a body large enough that the cancellation request reliably lands while flow analysis
        // is still running, even under parallel test execution load, without making parsing too
        // slow for the cancellation harness.
        for _ in 0..8_000_u32 {
            source.push_str("x = x;");
        }
        source.push_str("} }");
        db.set_file_text(file, source);

        // Prime `hir_item_tree` so the cancellation harness focuses on flow analysis.
        let tree = db.hir_item_tree(file);
        let (&method_ast_id, _) = tree
            .methods
            .iter()
            .find(|(_, method)| method.name == "m")
            .expect("expected Foo.m method");
        let method_id = nova_hir::ids::MethodId::new(file, method_ast_id);

        assert_query_is_cancelled(db, move |snap| {
            let _ = snap.flow_diagnostics(method_id);
        });
    }

    #[test]
    fn hir_queries_hit_cancellation_checkpoint() {
        use std::sync::mpsc;
        use std::time::Duration;

        let mut db = RootDatabase::default();
        let file = FileId::from_raw(1);
        db.set_file_text(file, "class Foo { int x; }");

        let (entered_tx, entered_rx) = mpsc::channel();
        let _guard =
            cancellation::test_support::install_entered_long_running_region_sender(entered_tx);

        // Any HIR query that performs loop checkpoints should trigger the test hook at least once.
        let _ = db.hir_symbol_names(file);
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("HIR query never hit a cancellation checkpoint (missing unwind_if_cancelled?)");
    }

    #[test]
    fn memoized_reads_increment_validated_memoized() {
        let mut db = RootDatabase::default();
        let file = FileId::from_raw(1);

        db.set_file_text(file, "class Foo {}");
        db.clear_query_stats();

        db.parse(file);
        let first = stat(&db, "parse");
        assert_eq!(first.executions, 1);
        assert_eq!(first.validated_memoized, 0);

        // Advance the revision without changing any inputs. The next read should
        // validate (and reuse) the memoized value.
        ra_salsa::Database::synthetic_write(&mut db, ra_salsa::Durability::LOW);
        db.parse(file);

        let second = stat(&db, "parse");
        assert_eq!(
            second.executions, 1,
            "expected memoized value to be reused after synthetic_write"
        );
        assert_eq!(second.validated_memoized, 1);

        // Editing an input should invalidate and re-execute the query.
        db.set_file_text(file, "class Foo { int x; }");
        db.parse(file);
        let third = stat(&db, "parse");
        assert_eq!(third.executions, 2);
    }

    #[test]
    fn concurrent_reads_record_blocking() {
        use std::sync::mpsc;

        let mut db = RootDatabase::default();
        let file = FileId::from_raw(1);
        db.set_source_root(file, SourceRootId::from_raw(0));
        db.set_file_text(file, "class Foo {}");
        db.clear_query_stats();

        let snap1 = db.snapshot();
        let snap2 = db.snapshot();

        let (entered_tx, entered_rx) = mpsc::channel();
        let h1 = std::thread::spawn(move || {
            let _guard =
                cancellation::test_support::install_entered_long_running_region_sender(entered_tx);
            snap1.interruptible_work(file, 2_000_000)
        });

        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("interruptible_work did not reach a cancellation checkpoint");

        let h2 = std::thread::spawn(move || snap2.interruptible_work(file, 2_000_000));

        let _ = h1.join().expect("snapshot 1 panicked");
        let _ = h2.join().expect("snapshot 2 panicked");

        let interrupt_stat = stat(&db, "interruptible_work");
        assert!(
            interrupt_stat.blocked_on_other_runtime > 0,
            "expected one runtime to block on another while computing the same query"
        );
    }

    #[test]
    fn wrapper_sets_default_source_root() {
        let db = Database::new();
        let file = FileId::from_raw(1);

        db.set_file_text(file, "class Foo {}".to_string());

        db.with_snapshot(|snap| {
            assert!(snap.file_exists(file));
            assert_eq!(snap.source_root(file), SourceRootId::from_raw(0));
        });
    }

    #[test]
    fn file_rel_path_is_shared_with_persistent_file_paths() {
        let db = Database::new();
        let file = FileId::from_raw(1);

        db.set_file_rel_path(file, Arc::new("src/A.java".to_string()));

        let snap = db.snapshot();
        let rel_path = snap.file_rel_path(file);
        let persistent_path = snap.file_path(file).expect("expected file path for FileId");

        assert_eq!(&*rel_path, &*persistent_path);
        assert!(
            Arc::ptr_eq(&rel_path, &persistent_path),
            "expected file_rel_path and file_path to share the same Arc"
        );
    }

    #[test]
    fn salsa_input_tracker_accounts_file_content_bytes() {
        let manager = MemoryManager::new(MemoryBudget::from_total(1_000));
        let db = Database::new();
        db.register_salsa_input_tracker(&manager);

        let file1 = FileId::from_raw(1);
        let file2 = FileId::from_raw(2);

        db.set_file_content(file1, Arc::new("abcd".to_string()));
        db.set_file_content(file2, Arc::new("hello!".to_string()));
        assert_eq!(
            manager.report().usage.other,
            4 + 6,
            "expected other usage to equal sum of file_content lengths"
        );

        // Replacing a file's content should update accounting incrementally.
        db.set_file_content(file1, Arc::new("a".to_string()));
        assert_eq!(manager.report().usage.other, 1 + 6);

        // `set_file_text` also updates `file_content` and should be tracked.
        db.set_file_text(file2, "xyz");
        let rel_path_bytes = format!("file-{}.java", file2.to_raw()).len() as u64;
        let project_files_bytes = std::mem::size_of::<FileId>() as u64;
        let expected = 1 + 3 + rel_path_bytes + project_files_bytes;
        assert_eq!(manager.report().usage.other, expected);
        assert_eq!(db.salsa_input_bytes(), expected);
    }

    #[test]
    fn salsa_input_tracker_updates_on_apply_file_text_edit() {
        let manager = MemoryManager::new(MemoryBudget::from_total(1_000));
        let db = Database::new();
        db.register_salsa_input_tracker(&manager);

        let file = FileId::from_raw(1);
        db.set_file_text(file, "abc");
        let rel_path_bytes = format!("file-{}.java", file.to_raw()).len() as u64;
        let project_files_bytes = std::mem::size_of::<FileId>() as u64;
        assert_eq!(
            manager.report().usage.other,
            3 + rel_path_bytes + project_files_bytes
        );

        let edit = nova_core::TextEdit::new(
            nova_core::TextRange::new(nova_core::TextSize::from(1), nova_core::TextSize::from(2)),
            "xxxx",
        );
        db.apply_file_text_edit(file, edit, Arc::new("axxxxc".to_string()));

        // "abc" -> replace "b" with "xxxx" => "axxxxc" (6 bytes).
        // Incremental parse metadata keeps the previous text snapshot and the edit replacement.
        let expected = (6 /* new file_content */ + 3 /* file_prev_content */ + 4/* replacement */)
            + rel_path_bytes
            + project_files_bytes;
        assert_eq!(manager.report().usage.other, expected);
        assert_eq!(db.salsa_input_bytes(), expected);
    }

    #[test]
    fn salsa_input_tracker_accounts_project_class_ids_bytes() {
        let manager = MemoryManager::new(MemoryBudget::from_total(1_000));
        let db = Database::new();
        db.register_salsa_input_tracker(&manager);

        let project = ProjectId::from_raw(0);
        let mapping = Arc::new(vec![
            (Arc::<str>::from("com.example.Foo"), ClassId::from_raw(1)),
            (Arc::<str>::from("com.example.Bar"), ClassId::from_raw(2)),
        ]);
        db.set_project_class_ids(project, Arc::clone(&mapping));

        let expected = (mapping.len() as u64) * (std::mem::size_of::<(Arc<str>, ClassId)>() as u64)
            + ("com.example.Foo".len() as u64)
            + ("com.example.Bar".len() as u64);
        assert_eq!(manager.report().usage.other, expected);
        assert_eq!(db.salsa_input_bytes(), expected);
    }

    #[test]
    fn salsa_input_tracker_accounts_project_config_bytes() {
        let manager = MemoryManager::new(MemoryBudget::from_total(1_000_000));
        let db = Database::new();
        db.register_salsa_input_tracker(&manager);

        let project = ProjectId::from_raw(0);
        let config = ProjectConfig {
            workspace_root: PathBuf::new(),
            build_system: BuildSystem::Simple,
            java: JavaConfig {
                source: JavaVersion::JAVA_21,
                target: JavaVersion::JAVA_21,
                enable_preview: false,
            },
            modules: vec![nova_project::Module {
                name: "m".to_string(),
                root: PathBuf::new(),
                annotation_processing: nova_project::AnnotationProcessing::default(),
            }],
            jpms_modules: Vec::new(),
            jpms_workspace: None,
            source_roots: Vec::new(),
            module_path: Vec::new(),
            classpath: Vec::new(),
            output_dirs: Vec::new(),
            dependencies: vec![nova_project::Dependency {
                group_id: "g".to_string(),
                artifact_id: "a".to_string(),
                version: None,
                scope: None,
                classifier: None,
                type_: None,
            }],
            workspace_model: None,
        };

        let expected = (std::mem::size_of::<ProjectConfig>()
            + std::mem::size_of::<nova_project::Module>()
            + std::mem::size_of::<nova_project::Dependency>()) as u64
            + 1 /* module name */
            + 1 /* group_id */
            + 1 /* artifact_id */;

        db.set_project_config(project, Arc::new(config));

        assert_eq!(manager.report().usage.other, expected);
        assert_eq!(db.salsa_input_bytes(), expected);
    }

    #[test]
    fn salsa_input_tracker_registers_via_memo_evictor_and_picks_up_existing_inputs() {
        let manager = MemoryManager::new(MemoryBudget::from_total(1_000));
        let db = Database::new();

        // Inputs set before registration should still be reflected once the tracker is registered.
        let file = FileId::from_raw(1);
        db.set_file_text(file, "abc");
        assert_eq!(
            manager.report().usage.other,
            0,
            "tracker is not registered yet"
        );

        // The workspace wires memory through memo eviction registration.
        db.register_salsa_memo_evictor(&manager);
        let rel_path_bytes = format!("file-{}.java", file.to_raw()).len() as u64;
        let project_files_bytes = std::mem::size_of::<FileId>() as u64;
        assert_eq!(
            manager.report().usage.other,
            3 + rel_path_bytes + project_files_bytes
        );
    }

    #[test]
    fn java_parse_cache_is_tracked_via_memo_evictor_registration() {
        let manager = MemoryManager::new(MemoryBudget::from_total(1_000));
        let db = Database::new();

        // Workspace initialization registers memory tracking/eviction via this hook.
        db.register_salsa_memo_evictor(&manager);

        let file = FileId::from_raw(1);
        let text = "class Foo { int x; }";
        db.set_file_text(file, text);
        db.with_snapshot(|snap| {
            let _ = snap.parse_java(file);
        });

        // The cache registers as an evictor/tracker, but intentionally reports 0 bytes to avoid
        // double-counting parse allocations that are also tracked by Salsa memo footprint.
        let (_report, components) = manager.report_detailed();
        let cache = components
            .iter()
            .find(|c| c.name == "java_parse_cache")
            .expect("expected java_parse_cache to be registered in MemoryManager");
        assert_eq!(cache.category, MemoryCategory::SyntaxTrees);
        assert_eq!(cache.bytes, 0);
    }

    #[test]
    fn salsa_input_jdk_index_is_tracked_in_memory_manager() {
        let manager = MemoryManager::new(MemoryBudget::from_total(10 * 1024 * 1024));
        let db = Database::new_with_memory_manager(&manager);
        let project = ProjectId::from_raw(0);

        assert_eq!(
            manager.report().usage.type_info,
            0,
            "expected no tracked index usage before setting inputs"
        );

        let jdk1 = Arc::new(nova_jdk::JdkIndex::new());
        let bytes1 = jdk1.estimated_bytes();
        db.set_jdk_index(project, jdk1);
        assert_eq!(
            manager.report().usage.type_info,
            bytes1,
            "expected memory manager to track jdk_index bytes"
        );

        // Replace with an empty index and ensure the tracker updates.
        let jdk2 = Arc::new(nova_jdk::JdkIndex::default());
        let bytes2 = jdk2.estimated_bytes();
        db.set_jdk_index(project, jdk2);
        assert_eq!(
            manager.report().usage.type_info,
            bytes2,
            "expected memory tracker to update when jdk_index is replaced"
        );
        assert!(bytes2 < bytes1, "expected replacement to reduce usage");
    }

    #[test]
    fn salsa_input_classpath_index_is_tracked_in_memory_manager() {
        let manager = MemoryManager::new(MemoryBudget::from_total(10 * 1024 * 1024));
        let db = Database::new_with_memory_manager(&manager);
        let project = ProjectId::from_raw(0);

        assert_eq!(
            manager.report().usage.type_info,
            0,
            "expected no tracked index usage before setting inputs"
        );

        let stub = nova_classpath::ClasspathClassStub {
            binary_name: "com.example.Foo".to_string(),
            internal_name: "com/example/Foo".to_string(),
            access_flags: 0,
            super_binary_name: None,
            interfaces: Vec::new(),
            signature: None,
            annotations: Vec::new(),
            fields: vec![nova_classpath::ClasspathFieldStub {
                name: "FOO".to_string(),
                descriptor: "I".to_string(),
                signature: None,
                access_flags: 0,
                annotations: Vec::new(),
            }],
            methods: vec![nova_classpath::ClasspathMethodStub {
                name: "bar".to_string(),
                descriptor: "()V".to_string(),
                signature: None,
                access_flags: 0,
                annotations: Vec::new(),
            }],
        };

        let module_aware = nova_classpath::ModuleAwareClasspathIndex::from_stubs([(stub, None)]);
        let classpath = module_aware.types;
        let bytes1 = classpath.estimated_bytes();
        db.set_classpath_index(project, Some(Arc::new(classpath)));
        assert_eq!(
            manager.report().usage.type_info,
            bytes1,
            "expected memory manager to track classpath_index bytes"
        );

        // Drop the classpath index and ensure usage returns to zero.
        db.set_classpath_index(project, None);
        assert_eq!(
            manager.report().usage.type_info,
            0,
            "expected memory tracker to update when classpath_index is cleared"
        );
    }

    #[test]
    fn salsa_input_classpath_index_evicts_under_memory_pressure() {
        let manager = MemoryManager::new(MemoryBudget::from_total(1_000));
        let db = Database::new_with_memory_manager(&manager);
        let project = ProjectId::from_raw(0);

        let stub = nova_classpath::ClasspathClassStub {
            binary_name: "com.example.Foo".to_string(),
            internal_name: "com/example/Foo".to_string(),
            access_flags: 0,
            super_binary_name: None,
            interfaces: Vec::new(),
            signature: None,
            annotations: Vec::new(),
            fields: vec![nova_classpath::ClasspathFieldStub {
                name: "FOO".to_string(),
                descriptor: "I".to_string(),
                signature: None,
                access_flags: 0,
                annotations: Vec::new(),
            }],
            methods: vec![nova_classpath::ClasspathMethodStub {
                name: "bar".to_string(),
                descriptor: "()V".to_string(),
                signature: None,
                access_flags: 0,
                annotations: Vec::new(),
            }],
        };

        let module_aware = nova_classpath::ModuleAwareClasspathIndex::from_stubs([(stub, None)]);
        let classpath = module_aware.types;
        db.set_classpath_index(project, Some(Arc::new(classpath)));

        assert!(
            db.snapshot().classpath_index(project).is_some(),
            "expected classpath_index to be set before eviction"
        );

        // Under very small budgets the process will enter Critical pressure due to RSS and should
        // evict the classpath index to keep the process alive.
        manager.enforce();

        assert!(
            db.snapshot().classpath_index(project).is_none(),
            "expected classpath_index to be cleared under memory pressure"
        );
        assert_eq!(
            manager.report().usage.type_info,
            0,
            "expected index usage to return to 0 after classpath eviction"
        );
    }

    #[test]
    fn salsa_input_jdk_index_evictor_clears_symbol_caches_under_pressure() {
        let manager = MemoryManager::new(MemoryBudget::from_total(1_000));
        let db = Database::new_with_memory_manager(&manager);
        let project = ProjectId::from_raw(0);

        let fake_jdk_root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../nova-jdk/testdata/fake-jdk");
        let fake_jdk_root = std::fs::canonicalize(&fake_jdk_root).unwrap_or(fake_jdk_root);

        let jdk = Arc::new(
            nova_jdk::JdkIndex::from_jdk_root_with_cache(fake_jdk_root, None)
                .expect("expected fake JDK fixture to load"),
        );
        db.set_jdk_index(project, Arc::clone(&jdk));

        // Warm the symbol caches by loading multiple stubs.
        assert!(jdk
            .lookup_type("java.lang.String")
            .expect("lookup should succeed")
            .is_some());
        assert!(jdk
            .lookup_type("java.util.List")
            .expect("lookup should succeed")
            .is_some());
        assert!(jdk
            .lookup_type("java.lang.Custom")
            .expect("lookup should succeed")
            .is_some());

        // Refresh the tracked bytes for the same `Arc` allocation now that caches are populated.
        db.set_jdk_index(project, Arc::clone(&jdk));
        let bytes_before = manager.report().usage.type_info;
        assert!(
            bytes_before > 0,
            "expected JDK index usage to be tracked after setting input"
        );

        manager.enforce();

        let bytes_after = manager.report().usage.type_info;
        assert!(
            bytes_after < bytes_before,
            "expected JDK index eviction to reduce tracked bytes (before={bytes_before}, after={bytes_after})"
        );

        // The index should remain usable after eviction (cache miss -> recompute).
        assert!(jdk
            .lookup_type("java.lang.String")
            .expect("lookup should succeed after eviction")
            .is_some());
    }

    #[test]
    fn salsa_memos_evict_under_memory_pressure_and_recompute() {
        let manager = MemoryManager::new(MemoryBudget::from_total(1_000));
        let db = Database::new_with_memory_manager(&manager);

        let files: Vec<FileId> = (0..128).map(FileId::from_raw).collect();
        for (idx, file) in files.iter().copied().enumerate() {
            db.set_file_text(
                file,
                format!("class C{idx} {{ int x = {idx}; int y = {idx}; }}"),
            );
        }

        db.with_snapshot(|snap| {
            for file in &files {
                let _ = snap.parse(*file);
                let _ = snap.item_tree(*file);
            }
        });

        let bytes_before = db.salsa_memo_bytes();
        assert!(
            bytes_before > 0,
            "expected memo tracker to grow after queries"
        );
        assert_eq!(
            manager.report().usage.query_cache,
            bytes_before,
            "memory manager should see tracked salsa memo usage"
        );
        let input_bytes_before = db.salsa_input_bytes();
        assert_eq!(
            manager.report().usage.other,
            input_bytes_before,
            "memory manager should see tracked salsa input usage"
        );

        let parse_exec_before = executions(&db.inner.lock(), "parse");
        let item_tree_exec_before = executions(&db.inner.lock(), "item_tree");

        // Validate that memoization is working prior to eviction.
        db.with_snapshot(|snap| {
            for file in &files {
                let _ = snap.parse(*file);
                let _ = snap.item_tree(*file);
            }
        });
        assert_eq!(
            executions(&db.inner.lock(), "parse"),
            parse_exec_before,
            "expected cached parse results prior to eviction"
        );
        assert_eq!(
            executions(&db.inner.lock(), "item_tree"),
            item_tree_exec_before,
            "expected cached item_tree results prior to eviction"
        );

        // Trigger an enforcement pass; the evictor should rebuild the database and
        // drop memoized results.
        manager.enforce();

        assert_eq!(
            db.salsa_memo_bytes(),
            0,
            "expected memo tracker to clear after eviction"
        );
        assert_eq!(
            manager.report().usage.other,
            input_bytes_before,
            "input bytes should remain stable across memo eviction rebuilds"
        );

        // Subsequent queries should recompute after eviction.
        let parse_exec_after_evict = executions(&db.inner.lock(), "parse");
        db.with_snapshot(|snap| {
            let _ = snap.parse(files[0]);
            let _ = snap.item_tree(files[0]);
        });
        assert!(
            executions(&db.inner.lock(), "parse") > parse_exec_after_evict,
            "expected parse to re-execute after memo eviction"
        );
    }

    #[test]
    fn salsa_memo_evictor_flush_to_disk_persists_project_indexes() {
        let tmp = TempDir::new().unwrap();
        let project_root = tmp.path().join("project");
        std::fs::create_dir_all(&project_root).unwrap();

        let cache_root = tmp.path().join("cache");
        std::fs::create_dir_all(&cache_root).unwrap();

        let manager = MemoryManager::new(MemoryBudget::from_total(1));
        let db = Database::new_with_persistence(
            &project_root,
            PersistenceConfig {
                mode: crate::persistence::PersistenceMode::ReadWrite,
                cache: CacheConfig {
                    cache_root_override: Some(cache_root.clone()),
                },
            },
        );
        db.register_salsa_memo_evictor(&manager);

        let project = ProjectId::from_raw(0);
        let file = FileId::from_raw(1);
        db.set_file_text(file, "class Foo { int x; }");
        db.set_file_rel_path(file, Arc::new("src/Foo.java".to_string()));
        db.set_project_files(project, Arc::new(vec![file]));

        // Ensure we have some tracked usage so `MemoryManager::enforce()` enters High/Critical on
        // platforms where RSS is unavailable.
        db.with_snapshot(|snap| {
            let _ = snap.parse(file);
        });
        assert!(
            manager.report().usage.query_cache > 0,
            "expected salsa memo tracker to report usage after parsing"
        );

        let cache_dir = nova_cache::CacheDir::new(
            &project_root,
            CacheConfig {
                cache_root_override: Some(cache_root),
            },
        )
        .unwrap();
        let expected_shard_artifact = cache_dir
            .indexes_dir()
            .join("shards")
            .join("0")
            .join("symbols.idx");
        assert!(
            !expected_shard_artifact.exists(),
            "expected index shards to be absent before flush"
        );

        // Under high/critical pressure, the memory manager should ask the salsa memo evictor to
        // flush cold artifacts before evicting memoized results.
        manager.enforce();

        assert!(
            expected_shard_artifact.exists(),
            "expected salsa memo evictor flush_to_disk to persist project index shards"
        );
    }

    #[test]
    fn open_doc_parse_is_not_double_counted_between_query_cache_and_syntax_trees() {
        use nova_syntax::SyntaxTreeStore;
        use nova_vfs::OpenDocuments;

        let manager = MemoryManager::new(MemoryBudget::from_total(10 * 1024 * 1024));
        let open_docs = Arc::new(OpenDocuments::default());
        let store = SyntaxTreeStore::new(&manager, open_docs.clone());

        let db = Database::new_with_memory_manager(&manager);
        db.set_syntax_tree_store(store);

        let file = FileId::from_raw(42);
        let text = "class Foo { int x; int y; int z; }\n".repeat(128);
        let text_len = text.len() as u64;

        db.set_file_text(file, text);
        open_docs.open(file);

        db.with_snapshot(|snap| {
            let parse = snap.parse(file);
            assert!(parse.errors.is_empty());
        });

        let report = manager.report();
        assert!(
            report.usage.syntax_trees > 0,
            "expected syntax tree store to report usage for open document"
        );

        // We expect the parse allocation to be accounted once across `query_cache`
        // and `syntax_trees` (not twice). Other categories (like tracked Salsa
        // input text) may legitimately contribute additional bytes.
        let syntax_and_cache = report
            .usage
            .query_cache
            .saturating_add(report.usage.syntax_trees);
        assert!(
            syntax_and_cache <= text_len.saturating_mul(3) / 2,
            "expected (query_cache+syntax_trees) to be ~text_len once (<= 1.5x), got sum={} (query_cache={}, syntax_trees={}) for text_len={text_len}",
            syntax_and_cache,
            report.usage.query_cache,
            report.usage.syntax_trees
        );
        assert!(
            report.usage.query_cache < text_len / 2,
            "expected query cache usage to not include the pinned parse allocation (query_cache={}, text_len={text_len})",
            report.usage.query_cache
        );
    }

    #[test]
    fn syntax_tree_store_eviction_restores_salsa_memo_bytes_for_open_documents() {
        use nova_memory::{EvictionRequest, MemoryEvictor};
        use nova_syntax::SyntaxTreeStore;
        use nova_vfs::OpenDocuments;

        let manager = MemoryManager::new(MemoryBudget::from_total(10 * 1024 * 1024));
        let open_docs = Arc::new(OpenDocuments::default());
        let store = SyntaxTreeStore::new(&manager, open_docs.clone());

        let db = Database::new_with_memory_manager(&manager);
        db.set_syntax_tree_store(store.clone());

        let file = FileId::from_raw(420);
        let text = "class Foo { int x; int y; int z; }\n".repeat(128);
        let text_len = text.len() as u64;

        db.set_file_text(file, text);
        open_docs.open(file);
        db.with_snapshot(|snap| {
            let parse = snap.parse(file);
            assert!(parse.errors.is_empty());
        });

        let pinned = manager.report();
        assert!(
            pinned.usage.query_cache < text_len / 2,
            "expected parse memo bytes to be suppressed while pinned (query_cache={}, text_len={text_len})",
            pinned.usage.query_cache
        );
        assert!(
            pinned.usage.syntax_trees > 0,
            "expected syntax tree store to report usage while pinned"
        );

        // Simulate critical memory eviction of the store: the pinned allocation is now only held
        // by Salsa memo tables and should be attributed back to `QueryCache`.
        store.evict(EvictionRequest {
            pressure: MemoryPressure::Critical,
            target_bytes: 0,
        });

        let after = manager.report();
        assert!(
            after.usage.syntax_trees < text_len / 4,
            "expected syntax tree store to be cleared after eviction (syntax_trees={}, text_len={text_len})",
            after.usage.syntax_trees
        );
        assert!(
            after.usage.query_cache >= text_len / 2,
            "expected parse memo bytes to be restored after store eviction (query_cache={}, text_len={text_len})",
            after.usage.query_cache
        );
    }

    #[test]
    fn syntax_tree_store_release_closed_files_restores_salsa_memo_bytes_for_closed_documents() {
        use nova_syntax::SyntaxTreeStore;
        use nova_vfs::OpenDocuments;

        let manager = MemoryManager::new(MemoryBudget::from_total(10 * 1024 * 1024));
        let open_docs = Arc::new(OpenDocuments::default());
        let store = SyntaxTreeStore::new(&manager, open_docs.clone());

        let db = Database::new_with_memory_manager(&manager);
        db.set_syntax_tree_store(store.clone());

        let file = FileId::from_raw(500);
        let text = "class Foo { int x; int y; int z; }\n".repeat(128);
        let text_len = text.len() as u64;

        db.set_file_text(file, text);
        open_docs.open(file);
        db.with_snapshot(|snap| {
            let parse = snap.parse(file);
            assert!(parse.errors.is_empty());
        });

        let pinned = manager.report();
        assert!(
            pinned.usage.query_cache < text_len / 2,
            "expected parse memo bytes to be suppressed while pinned (query_cache={}, text_len={text_len})",
            pinned.usage.query_cache
        );
        assert!(
            pinned.usage.syntax_trees > 0,
            "expected syntax tree store to report usage while pinned"
        );

        // Close without calling `Database::unpin_syntax_tree`. If the store releases closed files,
        // it should restore memo accounting via the on-remove callback.
        open_docs.close(file);
        store.release_closed_files();

        let after = manager.report();
        assert!(
            after.usage.syntax_trees < text_len / 4,
            "expected syntax tree store to release closed files (syntax_trees={}, text_len={text_len})",
            after.usage.syntax_trees
        );
        assert!(
            after.usage.query_cache >= text_len / 2,
            "expected parse memo bytes to be restored after releasing closed files (query_cache={}, text_len={text_len})",
            after.usage.query_cache
        );
    }

    #[test]
    fn java_parse_store_eviction_restores_salsa_memo_bytes_for_open_documents() {
        use nova_memory::{EvictionRequest, MemoryEvictor};
        use nova_syntax::JavaParseStore;
        use nova_vfs::OpenDocuments;

        let manager = MemoryManager::new(MemoryBudget::from_total(10 * 1024 * 1024));
        let open_docs = Arc::new(OpenDocuments::default());
        let store = JavaParseStore::new(&manager, open_docs.clone());

        let db = Database::new_with_memory_manager(&manager);
        db.set_java_parse_store(Some(store.clone()));

        let file = FileId::from_raw(421);
        let text = "class Foo { int x; int y; int z; }\n".repeat(128);
        let text_len = text.len() as u64;

        db.set_file_text(file, text);
        open_docs.open(file);
        db.with_snapshot(|snap| {
            let parse = snap.parse_java(file);
            assert!(parse.errors.is_empty());
        });

        let pinned = manager.report();
        assert!(
            pinned.usage.query_cache < text_len / 2,
            "expected parse_java memo bytes to be suppressed while pinned (query_cache={}, text_len={text_len})",
            pinned.usage.query_cache
        );
        assert!(
            pinned.usage.syntax_trees > 0,
            "expected java parse store to report usage while pinned"
        );

        // Simulate critical eviction of the store: the pinned allocation is now only held by
        // Salsa memo tables and should be attributed back to `QueryCache`.
        store.evict(EvictionRequest {
            pressure: MemoryPressure::Critical,
            target_bytes: 0,
        });

        let after = manager.report();
        assert!(
            after.usage.syntax_trees < text_len / 4,
            "expected java parse store to be cleared after eviction (syntax_trees={}, text_len={text_len})",
            after.usage.syntax_trees
        );
        assert!(
            after.usage.query_cache >= text_len / 2,
            "expected parse_java memo bytes to be restored after store eviction (query_cache={}, text_len={text_len})",
            after.usage.query_cache
        );
    }

    #[test]
    fn item_tree_store_eviction_restores_salsa_memo_bytes_for_open_documents() {
        use nova_memory::{EvictionRequest, MemoryEvictor};
        use nova_vfs::OpenDocuments;

        let manager = MemoryManager::new(MemoryBudget::from_total(10 * 1024 * 1024));
        let open_docs = Arc::new(OpenDocuments::default());

        let db = Database::new_with_memory_manager(&manager);
        let store = db.attach_item_tree_store(&manager, open_docs.clone());

        let file = FileId::from_raw(422);
        let text = "class Foo { int x; int y; int z; }\n".repeat(128);
        let text_len = text.len() as u64;

        db.set_file_text(file, text);
        open_docs.open(file);
        db.with_snapshot(|snap| {
            let it = snap.item_tree(file);
            assert!(!it.items.is_empty(), "expected item_tree to contain items");
        });

        let pinned = manager.report();
        assert!(
            pinned.usage.syntax_trees > 0,
            "expected item_tree store to report usage while pinned"
        );
        assert!(
            pinned.usage.query_cache < text_len.saturating_mul(3) / 2,
            "expected item_tree memo bytes to be suppressed while pinned (query_cache={}, text_len={text_len})",
            pinned.usage.query_cache
        );

        // Simulate critical eviction of the store: the pinned allocation is now only held by Salsa
        // memo tables and should be attributed back to `QueryCache`.
        store.evict(EvictionRequest {
            pressure: MemoryPressure::Critical,
            target_bytes: 0,
        });

        let after = manager.report();
        assert!(
            after.usage.syntax_trees < text_len / 4,
            "expected item_tree store to be cleared after eviction (syntax_trees={}, text_len={text_len})",
            after.usage.syntax_trees
        );
        assert!(
            after.usage.query_cache >= text_len.saturating_mul(3) / 2,
            "expected item_tree memo bytes to be restored after store eviction (query_cache={}, text_len={text_len})",
            after.usage.query_cache
        );
    }

    #[test]
    fn item_tree_store_release_closed_files_restores_salsa_memo_bytes_for_closed_documents() {
        use nova_vfs::OpenDocuments;

        let manager = MemoryManager::new(MemoryBudget::from_total(10 * 1024 * 1024));
        let open_docs = Arc::new(OpenDocuments::default());

        let db = Database::new_with_memory_manager(&manager);
        let store = db.attach_item_tree_store(&manager, open_docs.clone());

        let file = FileId::from_raw(502);
        let text = "class Foo { int x; int y; int z; }\n".repeat(128);
        let text_len = text.len() as u64;

        db.set_file_text(file, text);
        open_docs.open(file);
        db.with_snapshot(|snap| {
            let it = snap.item_tree(file);
            assert!(!it.items.is_empty(), "expected item_tree to contain items");
        });

        let pinned = manager.report();
        assert!(
            pinned.usage.query_cache < text_len.saturating_mul(3) / 2,
            "expected item_tree memo bytes to be suppressed while pinned (query_cache={}, text_len={text_len})",
            pinned.usage.query_cache
        );
        assert!(
            pinned.usage.syntax_trees > 0,
            "expected item_tree store to report usage while pinned"
        );

        // Close without calling `Database::unpin_item_tree`. If the store releases closed files,
        // it should restore memo accounting via the on-remove callback.
        open_docs.close(file);
        store.release_closed_files();

        let after = manager.report();
        assert!(
            after.usage.syntax_trees < text_len / 4,
            "expected item_tree store to release closed files (syntax_trees={}, text_len={text_len})",
            after.usage.syntax_trees
        );
        assert!(
            after.usage.query_cache >= text_len.saturating_mul(3) / 2,
            "expected item_tree memo bytes to be restored after releasing closed files (query_cache={}, text_len={text_len})",
            after.usage.query_cache
        );
    }

    #[test]
    fn java_parse_store_release_closed_files_restores_salsa_memo_bytes_for_closed_documents() {
        use nova_syntax::JavaParseStore;
        use nova_vfs::OpenDocuments;

        let manager = MemoryManager::new(MemoryBudget::from_total(10 * 1024 * 1024));
        let open_docs = Arc::new(OpenDocuments::default());
        let store = JavaParseStore::new(&manager, open_docs.clone());

        let db = Database::new_with_memory_manager(&manager);
        db.set_java_parse_store(Some(store.clone()));

        let file = FileId::from_raw(501);
        let text = "class Foo { int x; int y; int z; }\n".repeat(128);
        let text_len = text.len() as u64;

        db.set_file_text(file, text);
        open_docs.open(file);
        db.with_snapshot(|snap| {
            let parse = snap.parse_java(file);
            assert!(parse.errors.is_empty());
        });

        let pinned = manager.report();
        assert!(
            pinned.usage.query_cache < text_len / 2,
            "expected parse_java memo bytes to be suppressed while pinned (query_cache={}, text_len={text_len})",
            pinned.usage.query_cache
        );
        assert!(
            pinned.usage.syntax_trees > 0,
            "expected java parse store to report usage while pinned"
        );

        // Close without calling `Database::unpin_java_parse_tree`. If the store releases closed
        // files, it should restore memo accounting via the on-remove callback.
        open_docs.close(file);
        store.release_closed_files();

        let after = manager.report();
        assert!(
            after.usage.syntax_trees < text_len / 4,
            "expected java parse store to release closed files (syntax_trees={}, text_len={text_len})",
            after.usage.syntax_trees
        );
        assert!(
            after.usage.query_cache >= text_len / 2,
            "expected parse_java memo bytes to be restored after releasing closed files (query_cache={}, text_len={text_len})",
            after.usage.query_cache
        );
    }

    #[test]
    fn open_doc_parse_java_is_not_double_counted_between_query_cache_and_syntax_trees() {
        use nova_syntax::JavaParseStore;
        use nova_vfs::OpenDocuments;

        let manager = MemoryManager::new(MemoryBudget::from_total(10 * 1024 * 1024));
        let open_docs = Arc::new(OpenDocuments::default());
        let store = JavaParseStore::new(&manager, open_docs.clone());

        let db = Database::new_with_memory_manager(&manager);
        db.set_java_parse_store(Some(store));

        let file = FileId::from_raw(44);
        let text = "class Foo { int x; int y; int z; }\n".repeat(128);
        let text_len = text.len() as u64;

        db.set_file_text(file, text);
        open_docs.open(file);

        db.with_snapshot(|snap| {
            let parse = snap.parse_java(file);
            assert!(parse.errors.is_empty());
        });

        let report = manager.report();
        assert!(
            report.usage.syntax_trees > 0,
            "expected java parse store to report usage for open document"
        );

        let syntax_and_cache = report
            .usage
            .query_cache
            .saturating_add(report.usage.syntax_trees);
        assert!(
            syntax_and_cache <= text_len.saturating_mul(3) / 2,
            "expected (query_cache+syntax_trees) to be ~text_len once (<= 1.5x), got sum={} (query_cache={}, syntax_trees={}) for text_len={text_len}",
            syntax_and_cache,
            report.usage.query_cache,
            report.usage.syntax_trees
        );
        assert!(
            report.usage.query_cache < text_len / 2,
            "expected query cache usage to not include the pinned parse_java allocation (query_cache={}, text_len={text_len})",
            report.usage.query_cache
        );
    }

    #[test]
    fn unpin_java_parse_tree_restores_salsa_memo_bytes() {
        use nova_syntax::JavaParseStore;
        use nova_vfs::OpenDocuments;

        let manager = MemoryManager::new(MemoryBudget::from_total(10 * 1024 * 1024));
        let open_docs = Arc::new(OpenDocuments::default());
        let store = JavaParseStore::new(&manager, open_docs.clone());

        let db = Database::new_with_memory_manager(&manager);
        db.set_java_parse_store(Some(store));

        let file = FileId::from_raw(45);
        let text = "class Foo { int x; int y; int z; }\n".repeat(128);
        let text_len = text.len() as u64;

        db.set_file_text(file, text);
        open_docs.open(file);

        db.with_snapshot(|snap| {
            let parse = snap.parse_java(file);
            assert!(parse.errors.is_empty());
        });

        let pinned_report = manager.report();
        assert!(
            pinned_report.usage.syntax_trees > 0,
            "expected java parse store to report usage for pinned file"
        );
        assert!(
            pinned_report.usage.query_cache < text_len / 2,
            "expected parse_java memo bytes to be suppressed while pinned (query_cache={}, text_len={text_len})",
            pinned_report.usage.query_cache
        );

        open_docs.close(file);
        db.unpin_java_parse_tree(file);

        let after_unpin_report = manager.report();
        assert!(
            after_unpin_report.usage.query_cache >= text_len / 2,
            "expected parse_java memo bytes to be restored after unpin (query_cache={}, text_len={text_len})",
            after_unpin_report.usage.query_cache
        );
        assert!(
            after_unpin_report.usage.syntax_trees < text_len / 4,
            "expected parse_java store bytes to be released after unpin (syntax_trees={}, text_len={text_len})",
            after_unpin_report.usage.syntax_trees
        );
    }

    #[test]
    fn open_doc_item_tree_is_not_double_counted_between_query_cache_and_syntax_trees() {
        use nova_syntax::SyntaxTreeStore;
        use nova_vfs::OpenDocuments;

        let manager = MemoryManager::new(MemoryBudget::from_total(10 * 1024 * 1024));
        let open_docs = Arc::new(OpenDocuments::default());
        let syntax_store = SyntaxTreeStore::new(&manager, open_docs.clone());

        let db = Database::new_with_memory_manager(&manager);
        db.set_syntax_tree_store(syntax_store);
        db.attach_item_tree_store(&manager, open_docs.clone());

        let file = FileId::from_raw(43);
        let text = "class Foo { int x; int y; int z; }\n".repeat(128);
        let text_len = text.len() as u64;

        db.set_file_text(file, text);
        open_docs.open(file);

        db.with_snapshot(|snap| {
            let it = snap.item_tree(file);
            assert!(
                !it.items.is_empty(),
                "expected item tree to contain at least one item"
            );
        });

        let report = manager.report();
        assert!(
            report.usage.syntax_trees > 0,
            "expected syntax tree + item tree stores to report usage for open document"
        );

        let syntax_and_cache = report
            .usage
            .query_cache
            .saturating_add(report.usage.syntax_trees);
        assert!(
            syntax_and_cache <= text_len.saturating_mul(5) / 2,
            "expected (query_cache+syntax_trees) to be ~2x text_len (<= 2.5x) when parse + item_tree are pinned, got sum={} (query_cache={}, syntax_trees={}) for text_len={text_len}",
            syntax_and_cache,
            report.usage.query_cache,
            report.usage.syntax_trees
        );
        assert!(
            report.usage.query_cache < text_len / 2,
            "expected query cache usage to not include pinned parse/item_tree allocations (query_cache={}, text_len={text_len})",
            report.usage.query_cache
        );
    }

    #[test]
    fn salsa_memo_footprint_tracks_hir_and_indexing_memos() {
        let manager = MemoryManager::new(MemoryBudget::from_total(1_000));
        let db = Database::new_with_memory_manager(&manager);
        let project = ProjectId::from_raw(0);
        db.set_jdk_index(project, Arc::new(nova_jdk::JdkIndex::new()));

        let files: Vec<FileId> = (0..64).map(FileId::from_raw).collect();
        for (idx, file) in files.iter().copied().enumerate() {
            db.set_file_text(
                file,
                format!(
                    "package test;\nimport java.util.List;\nimport static java.lang.Math.max;\nclass C{idx} {{ int x = {idx}; int y = {idx}; int foo(int a) {{ int b = a + x; return b; }} }}"
                ),
            );
            db.set_file_rel_path(file, Arc::new(format!("src/C{idx}.java")));
        }

        // Baseline: queries that were historically tracked.
        db.with_snapshot(|snap| {
            for file in &files {
                let _ = snap.parse(*file);
                let _ = snap.parse_java(*file);
                let _ = snap.item_tree(*file);
            }
        });

        let baseline_bytes = db.salsa_memo_bytes();
        assert!(baseline_bytes > 0, "expected baseline memo bytes to be > 0");

        // New tracking: lightweight Java parses.
        db.with_snapshot(|snap| {
            for file in &files {
                let _ = snap.java_parse(*file);
            }
        });

        let after_java_parse_bytes = db.salsa_memo_bytes();
        assert!(
            after_java_parse_bytes > baseline_bytes,
            "expected java_parse memos to increase tracked bytes"
        );

        // New tracking: HIR AST id maps.
        db.with_snapshot(|snap| {
            for file in &files {
                let _ = snap.hir_ast_id_map(*file);
            }
        });

        let after_ast_id_bytes = db.salsa_memo_bytes();
        assert!(
            after_ast_id_bytes > after_java_parse_bytes,
            "expected hir_ast_id_map memos to increase tracked bytes"
        );

        // New tracking: HIR item trees.
        db.with_snapshot(|snap| {
            for file in &files {
                let _ = snap.hir_item_tree(*file);
            }
        });

        let after_hir_bytes = db.salsa_memo_bytes();
        assert!(
            after_hir_bytes > after_ast_id_bytes,
            "expected hir_item_tree memos to increase tracked bytes"
        );

        // New tracking: scope graphs.
        db.with_snapshot(|snap| {
            for file in &files {
                let _ = snap.scope_graph(*file);
            }
        });

        let after_scope_bytes = db.salsa_memo_bytes();
        assert!(
            after_scope_bytes > after_hir_bytes,
            "expected scope_graph memos to increase tracked bytes"
        );

        // New tracking: per-file definition maps.
        db.with_snapshot(|snap| {
            for file in &files {
                let _ = snap.def_map(*file);
            }
        });

        let after_def_map_bytes = db.salsa_memo_bytes();
        assert!(
            after_def_map_bytes > after_scope_bytes,
            "expected def_map memos to increase tracked bytes"
        );

        // New tracking: per-file import maps.
        db.with_snapshot(|snap| {
            for file in &files {
                let _ = snap.import_map(*file);
            }
        });

        let after_import_map_bytes = db.salsa_memo_bytes();
        assert!(
            after_import_map_bytes > after_def_map_bytes,
            "expected import_map memos to increase tracked bytes"
        );

        // New tracking: per-file index deltas.
        db.with_snapshot(|snap| {
            for file in &files {
                let _ = snap.file_index_delta(*file);
            }
        });

        let after_index_bytes = db.salsa_memo_bytes();
        assert!(
            after_index_bytes > after_import_map_bytes,
            "expected file_index_delta memos to increase tracked bytes"
        );

        // New tracking: project-wide index shards + merged indexes.
        db.with_snapshot(|snap| {
            let _ = snap.project_indexes(project);
        });

        let after_project_bytes = db.salsa_memo_bytes();
        assert!(
            after_project_bytes > after_index_bytes,
            "expected project_indexes memos to increase tracked bytes"
        );

        // New tracking: workspace definition map.
        db.with_snapshot(|snap| {
            let _ = snap.workspace_def_map(project);
        });

        let after_workspace_bytes = db.salsa_memo_bytes();
        assert!(
            after_workspace_bytes > after_project_bytes,
            "expected workspace_def_map memos to increase tracked bytes"
        );

        let (method, owner) = db.with_snapshot(|snap| {
            let tree = snap.hir_item_tree(files[0]);
            let ast_id = *tree
                .methods
                .keys()
                .min()
                .expect("expected at least one method in test file");
            let method = nova_hir::ids::MethodId::new(files[0], ast_id);
            let owner = nova_resolve::ids::DefWithBodyId::Method(method);
            (method, owner)
        });

        // New tracking: body-level HIR.
        db.with_snapshot(|snap| {
            let _ = snap.hir_body(method);
        });

        let after_hir_body_bytes = db.salsa_memo_bytes();
        assert!(
            after_hir_body_bytes > after_workspace_bytes,
            "expected hir_body memos to increase tracked bytes"
        );

        // New tracking: per-body ExprScopes.
        db.with_snapshot(|snap| {
            let _ = snap.expr_scopes(owner);
        });

        let after_expr_scopes_bytes = db.salsa_memo_bytes();
        assert!(
            after_expr_scopes_bytes > after_hir_body_bytes,
            "expected expr_scopes memos to increase tracked bytes"
        );

        // New tracking: per-body type checking results and project base TypeStore.
        db.with_snapshot(|snap| {
            let _ = snap.typeck_body(owner);
        });

        let after_typeck_bytes = db.salsa_memo_bytes();
        assert!(
            after_typeck_bytes > after_expr_scopes_bytes,
            "expected typeck_body/project_base_type_store memos to increase tracked bytes"
        );

        // New tracking: flow IR + CFG.
        db.with_snapshot(|snap| {
            let _ = snap.flow_body(method);
            let _ = snap.cfg(method);
        });

        let after_flow_bytes = db.salsa_memo_bytes();
        assert!(
            after_flow_bytes > after_typeck_bytes,
            "expected flow_body/cfg memos to increase tracked bytes"
        );

        assert_eq!(
            manager.report().usage.query_cache,
            after_flow_bytes,
            "memory manager should see tracked salsa memo usage (including HIR + indexing)"
        );

        let hir_exec_before = executions(&db.inner.lock(), "hir_item_tree");
        let scope_exec_before = executions(&db.inner.lock(), "scope_graph");
        let def_map_exec_before = executions(&db.inner.lock(), "def_map");
        let import_map_exec_before = executions(&db.inner.lock(), "import_map");
        let hir_body_exec_before = executions(&db.inner.lock(), "hir_body");
        let expr_scopes_exec_before = executions(&db.inner.lock(), "expr_scopes");
        let base_store_exec_before = executions(&db.inner.lock(), "project_base_type_store");
        let typeck_exec_before = executions(&db.inner.lock(), "typeck_body");
        let flow_body_exec_before = executions(&db.inner.lock(), "flow_body");
        let cfg_exec_before = executions(&db.inner.lock(), "cfg");
        let delta_exec_before = executions(&db.inner.lock(), "file_index_delta");
        let shard_exec_before = executions(&db.inner.lock(), "project_index_shards");
        let project_exec_before = executions(&db.inner.lock(), "project_indexes");
        let workspace_exec_before = executions(&db.inner.lock(), "workspace_def_map");

        // Validate memoization before eviction.
        db.with_snapshot(|snap| {
            for file in &files {
                let _ = snap.hir_item_tree(*file);
                let _ = snap.scope_graph(*file);
                let _ = snap.def_map(*file);
                let _ = snap.import_map(*file);
                let _ = snap.file_index_delta(*file);
            }
            let _ = snap.project_indexes(project);
            let _ = snap.workspace_def_map(project);
            let _ = snap.hir_body(method);
            let _ = snap.expr_scopes(owner);
            let _ = snap.project_base_type_store(project);
            let _ = snap.typeck_body(owner);
            let _ = snap.flow_body(method);
            let _ = snap.cfg(method);
        });
        assert_eq!(
            executions(&db.inner.lock(), "hir_item_tree"),
            hir_exec_before,
            "expected cached hir_item_tree results prior to eviction"
        );
        assert_eq!(
            executions(&db.inner.lock(), "file_index_delta"),
            delta_exec_before,
            "expected cached file_index_delta results prior to eviction"
        );
        assert_eq!(
            executions(&db.inner.lock(), "scope_graph"),
            scope_exec_before,
            "expected cached scope_graph results prior to eviction"
        );
        assert_eq!(
            executions(&db.inner.lock(), "def_map"),
            def_map_exec_before,
            "expected cached def_map results prior to eviction"
        );
        assert_eq!(
            executions(&db.inner.lock(), "import_map"),
            import_map_exec_before,
            "expected cached import_map results prior to eviction"
        );
        assert_eq!(
            executions(&db.inner.lock(), "project_index_shards"),
            shard_exec_before,
            "expected cached project_index_shards results prior to eviction"
        );
        assert_eq!(
            executions(&db.inner.lock(), "project_indexes"),
            project_exec_before,
            "expected cached project_indexes results prior to eviction"
        );
        assert_eq!(
            executions(&db.inner.lock(), "workspace_def_map"),
            workspace_exec_before,
            "expected cached workspace_def_map results prior to eviction"
        );
        assert_eq!(
            executions(&db.inner.lock(), "hir_body"),
            hir_body_exec_before,
            "expected cached hir_body results prior to eviction"
        );
        assert_eq!(
            executions(&db.inner.lock(), "expr_scopes"),
            expr_scopes_exec_before,
            "expected cached expr_scopes results prior to eviction"
        );
        assert_eq!(
            executions(&db.inner.lock(), "project_base_type_store"),
            base_store_exec_before,
            "expected cached project_base_type_store results prior to eviction"
        );
        assert_eq!(
            executions(&db.inner.lock(), "typeck_body"),
            typeck_exec_before,
            "expected cached typeck_body results prior to eviction"
        );
        assert_eq!(
            executions(&db.inner.lock(), "flow_body"),
            flow_body_exec_before,
            "expected cached flow_body results prior to eviction"
        );
        assert_eq!(
            executions(&db.inner.lock(), "cfg"),
            cfg_exec_before,
            "expected cached cfg results prior to eviction"
        );

        manager.enforce();

        assert_eq!(
            db.salsa_memo_bytes(),
            0,
            "expected memo tracker to clear after eviction"
        );

        // Ensure queries recompute after eviction.
        let hir_exec_after_evict = executions(&db.inner.lock(), "hir_item_tree");
        let scope_exec_after_evict = executions(&db.inner.lock(), "scope_graph");
        let def_map_exec_after_evict = executions(&db.inner.lock(), "def_map");
        let import_map_exec_after_evict = executions(&db.inner.lock(), "import_map");
        let hir_body_exec_after_evict = executions(&db.inner.lock(), "hir_body");
        let expr_scopes_exec_after_evict = executions(&db.inner.lock(), "expr_scopes");
        let base_store_exec_after_evict = executions(&db.inner.lock(), "project_base_type_store");
        let typeck_exec_after_evict = executions(&db.inner.lock(), "typeck_body");
        let flow_body_exec_after_evict = executions(&db.inner.lock(), "flow_body");
        let cfg_exec_after_evict = executions(&db.inner.lock(), "cfg");
        let delta_exec_after_evict = executions(&db.inner.lock(), "file_index_delta");
        let shard_exec_after_evict = executions(&db.inner.lock(), "project_index_shards");
        let project_exec_after_evict = executions(&db.inner.lock(), "project_indexes");
        let workspace_exec_after_evict = executions(&db.inner.lock(), "workspace_def_map");
        db.with_snapshot(|snap| {
            let _ = snap.hir_item_tree(files[0]);
            let _ = snap.scope_graph(files[0]);
            let _ = snap.def_map(files[0]);
            let _ = snap.import_map(files[0]);
            let _ = snap.file_index_delta(files[0]);
            let _ = snap.project_indexes(project);
            let _ = snap.workspace_def_map(project);
            let _ = snap.hir_body(method);
            let _ = snap.expr_scopes(owner);
            let _ = snap.project_base_type_store(project);
            let _ = snap.typeck_body(owner);
            let _ = snap.flow_body(method);
            let _ = snap.cfg(method);
        });
        assert!(
            executions(&db.inner.lock(), "hir_item_tree") > hir_exec_after_evict,
            "expected hir_item_tree to re-execute after memo eviction"
        );
        assert!(
            executions(&db.inner.lock(), "file_index_delta") > delta_exec_after_evict,
            "expected file_index_delta to re-execute after memo eviction"
        );
        assert!(
            executions(&db.inner.lock(), "scope_graph") > scope_exec_after_evict,
            "expected scope_graph to re-execute after memo eviction"
        );
        assert!(
            executions(&db.inner.lock(), "def_map") > def_map_exec_after_evict,
            "expected def_map to re-execute after memo eviction"
        );
        assert!(
            executions(&db.inner.lock(), "import_map") > import_map_exec_after_evict,
            "expected import_map to re-execute after memo eviction"
        );
        assert!(
            executions(&db.inner.lock(), "project_index_shards") > shard_exec_after_evict,
            "expected project_index_shards to re-execute after memo eviction"
        );
        assert!(
            executions(&db.inner.lock(), "project_indexes") > project_exec_after_evict,
            "expected project_indexes to re-execute after memo eviction"
        );
        assert!(
            executions(&db.inner.lock(), "workspace_def_map") > workspace_exec_after_evict,
            "expected workspace_def_map to re-execute after memo eviction"
        );
        assert!(
            executions(&db.inner.lock(), "hir_body") > hir_body_exec_after_evict,
            "expected hir_body to re-execute after memo eviction"
        );
        assert!(
            executions(&db.inner.lock(), "expr_scopes") > expr_scopes_exec_after_evict,
            "expected expr_scopes to re-execute after memo eviction"
        );
        assert!(
            executions(&db.inner.lock(), "project_base_type_store") > base_store_exec_after_evict,
            "expected project_base_type_store to re-execute after memo eviction"
        );
        assert!(
            executions(&db.inner.lock(), "typeck_body") > typeck_exec_after_evict,
            "expected typeck_body to re-execute after memo eviction"
        );
        assert!(
            executions(&db.inner.lock(), "flow_body") > flow_body_exec_after_evict,
            "expected flow_body to re-execute after memo eviction"
        );
        assert!(
            executions(&db.inner.lock(), "cfg") > cfg_exec_after_evict,
            "expected cfg to re-execute after memo eviction"
        );
    }

    #[test]
    fn java_parse_cache_is_not_double_counted_between_query_cache_and_syntax_trees() {
        let manager = MemoryManager::new(MemoryBudget::from_total(10 * 1024 * 1024));
        let db = Database::new_with_memory_manager(&manager);

        let file = FileId::from_raw(99);
        let text = "class Foo { int x; int y; int z; }\n".repeat(128);
        let text_len = text.len() as u64;
        db.set_file_text(file, text);

        db.with_snapshot(|snap| {
            let parse = snap.parse_java(file);
            assert!(
                parse.errors.is_empty(),
                "expected Java parse errors to be empty, got: {:?}",
                parse.errors
            );
        });

        let report = manager.report();
        let syntax_and_cache = report
            .usage
            .query_cache
            .saturating_add(report.usage.syntax_trees);
        assert!(
            syntax_and_cache <= text_len.saturating_mul(3) / 2,
            "expected (query_cache+syntax_trees) to be ~text_len once (<= 1.5x), got sum={} (query_cache={}, syntax_trees={}) for text_len={text_len}",
            syntax_and_cache,
            report.usage.query_cache,
            report.usage.syntax_trees
        );
        assert!(
            report.usage.query_cache >= text_len / 2,
            "expected query cache usage to include parse_java memo bytes (query_cache={}, text_len={text_len})",
            report.usage.query_cache
        );
        assert!(
            report.usage.syntax_trees < text_len / 4,
            "expected java_parse_cache to not double-count parse_java bytes under syntax_trees (syntax_trees={}, text_len={text_len})",
            report.usage.syntax_trees
        );
    }

    #[test]
    fn salsa_memo_eviction_preserves_snapshot_results() {
        let manager = MemoryManager::new(MemoryBudget::from_total(1_000));
        let db = Database::new_with_memory_manager(&manager);
        let file = FileId::from_raw(1);
        db.set_file_text(file, "class Foo { int x; }");

        let snap = db.snapshot();
        let parse_from_snapshot = snap.parse(file);
        assert!(parse_from_snapshot.errors.is_empty());

        // Evict memoized values from the main database while the snapshot is alive.
        db.evict_salsa_memos(MemoryPressure::Critical);

        // Previously returned results remain valid and the snapshot stays usable.
        assert_eq!(&*parse_from_snapshot, &*snap.parse(file));
    }

    #[test]
    fn evict_salsa_memos_preserves_inputs_and_recomputes() {
        let db = Database::new();

        let files = [
            FileId::from_raw(1),
            FileId::from_raw(2),
            FileId::from_raw(3),
        ];
        let texts = [
            "class Foo { int x; }",
            "class Bar { int y; }",
            "class Baz { int z; }",
        ];

        for (file, text) in files.iter().copied().zip(texts) {
            db.set_file_text(file, text);
        }

        let snap = db.snapshot();
        let counts_before: Vec<_> = files.iter().map(|&file| snap.symbol_count(file)).collect();
        let parses_before: Vec<_> = files.iter().map(|&file| snap.parse(file)).collect();

        let parse_exec_before_evict = executions(&db.inner.lock(), "parse");

        // Evict memoized values from the main database while the snapshot is alive.
        db.evict_salsa_memos(MemoryPressure::Critical);

        // Previously returned results remain valid and the snapshot stays usable.
        for (idx, file) in files.iter().copied().enumerate() {
            assert_eq!(&*parses_before[idx], &*snap.parse(file));
            assert_eq!(counts_before[idx], snap.symbol_count(file));
        }

        // Inputs should still be present in the rebuilt database.
        let snap2 = db.snapshot();
        for (file, expected) in files.iter().copied().zip(texts) {
            assert_eq!(snap2.file_content(file).as_str(), expected);
        }

        // Derived queries should recompute against the rebuilt database and produce
        // the same results.
        for (idx, file) in files.iter().copied().enumerate() {
            assert!(snap2.parse(file).errors.is_empty());
            assert_eq!(counts_before[idx], snap2.symbol_count(file));
        }
        assert!(
            executions(&db.inner.lock(), "parse") > parse_exec_before_evict,
            "expected parse to re-execute after memo eviction"
        );
    }

    #[test]
    fn open_documents_pin_item_tree_across_salsa_memo_eviction() {
        let manager = MemoryManager::new(MemoryBudget::from_total(1_000));
        let db = Database::new_with_memory_manager(&manager);

        let open_docs = Arc::new(OpenDocuments::default());
        db.attach_item_tree_store(&manager, open_docs.clone());

        let file = FileId::from_raw(1);
        open_docs.open(file);
        db.set_file_text(file, "class Foo { int x; }");

        let it_before = db.with_snapshot(|snap| snap.item_tree(file));

        // Rebuild the underlying Salsa DB to drop memoized results.
        db.evict_salsa_memos(MemoryPressure::Critical);

        let it_after = db.with_snapshot(|snap| snap.item_tree(file));
        assert!(
            Arc::ptr_eq(&it_before, &it_after),
            "expected open-document ItemTreeStore to reuse item_tree result across memo eviction"
        );
    }

    #[test]
    fn open_document_item_tree_cache_respects_file_text_identity() {
        let manager = MemoryManager::new(MemoryBudget::from_total(1_000_000));
        let open_docs = Arc::new(OpenDocuments::default());

        let db = Database::new_with_memory_manager(&manager);
        db.attach_item_tree_store(&manager, open_docs.clone());

        let file = FileId::from_raw(1);
        open_docs.open(file);

        let text1 = Arc::new("class Foo {}".to_string());
        db.set_file_exists(file, true);
        db.set_file_content(file, text1.clone());

        let it1 = db.snapshot().item_tree(file);
        assert_eq!(it1.items.len(), 1);
        assert_eq!(it1.items[0].name, "Foo");

        // Pinned item_tree should survive memo eviction while the text identity matches.
        db.evict_salsa_memos(MemoryPressure::Critical);
        let it1_after_evict = db.snapshot().item_tree(file);
        assert!(
            Arc::ptr_eq(&it1, &it1_after_evict),
            "expected open document item_tree to be reused across eviction when text is unchanged"
        );

        // Changing the file text should invalidate the pinned item_tree (even if the file is still
        // open) to avoid returning stale trees.
        let text2 = Arc::new("class Bar {}".to_string());
        db.set_file_content(file, text2.clone());
        let it2 = db.snapshot().item_tree(file);
        assert!(
            !Arc::ptr_eq(&it1, &it2),
            "expected item_tree to recompute after file_content changes"
        );
        assert_eq!(it2.items.len(), 1);
        assert_eq!(it2.items[0].name, "Bar");

        // The new item_tree should now be pinned and survive a subsequent eviction.
        db.evict_salsa_memos(MemoryPressure::Critical);
        let it2_after_evict = db.snapshot().item_tree(file);
        assert!(
            Arc::ptr_eq(&it2, &it2_after_evict),
            "expected new open document item_tree to be reused across eviction"
        );
    }

    #[test]
    fn open_document_reuses_parse_after_salsa_memo_eviction() {
        let manager = MemoryManager::new(MemoryBudget::from_total(1_000_000));
        let open_docs = Arc::new(OpenDocuments::default());
        let file = FileId::from_raw(1);
        open_docs.open(file);

        let store = SyntaxTreeStore::new(&manager, open_docs);
        let db = Database::new_with_memory_manager(&manager);
        db.set_syntax_tree_store(store);

        let text = Arc::new("class Foo { int x; }".to_string());
        db.set_file_exists(file, true);
        db.set_file_content(file, text);

        let before = db.snapshot().parse(file);
        db.evict_salsa_memos(MemoryPressure::Critical);
        let after = db.snapshot().parse(file);

        assert!(
            Arc::ptr_eq(&before, &after),
            "expected open document parse to be reused after memo eviction"
        );
    }

    #[test]
    fn open_document_parse_cache_respects_file_text_identity() {
        let manager = MemoryManager::new(MemoryBudget::from_total(1_000_000));
        let open_docs = Arc::new(OpenDocuments::default());
        let file = FileId::from_raw(1);
        open_docs.open(file);

        let store = SyntaxTreeStore::new(&manager, open_docs);
        let db = Database::new_with_memory_manager(&manager);
        db.set_syntax_tree_store(store);

        let text1 = Arc::new("class Foo {}".to_string());
        db.set_file_exists(file, true);
        db.set_file_content(file, text1.clone());

        let parse1 = db.snapshot().parse(file);
        assert_eq!(parse1.root.text_len as usize, text1.len());

        // Pinned parse should survive memo eviction while the text identity matches.
        db.evict_salsa_memos(MemoryPressure::Critical);
        let parse1_after_evict = db.snapshot().parse(file);
        assert!(
            Arc::ptr_eq(&parse1, &parse1_after_evict),
            "expected open document parse to be reused across eviction when text is unchanged"
        );

        // Changing the file text should invalidate the pinned parse (even if the file is still
        // open) to avoid returning stale trees.
        let text2 = Arc::new("class Foo { int x; }".to_string());
        db.set_file_content(file, text2.clone());
        let parse2 = db.snapshot().parse(file);
        assert!(
            !Arc::ptr_eq(&parse1, &parse2),
            "expected parse to recompute after file_content changes"
        );
        assert_eq!(parse2.root.text_len as usize, text2.len());

        // The new parse should now be pinned and survive a subsequent eviction.
        db.evict_salsa_memos(MemoryPressure::Critical);
        let parse2_after_evict = db.snapshot().parse(file);
        assert!(
            Arc::ptr_eq(&parse2, &parse2_after_evict),
            "expected new open document parse to be reused across eviction"
        );
    }

    #[test]
    fn open_document_parse_java_cache_respects_file_text_identity() {
        use nova_syntax::JavaParseStore;
        use nova_vfs::OpenDocuments;

        let manager = MemoryManager::new(MemoryBudget::from_total(1_000_000));
        let open_docs = Arc::new(OpenDocuments::default());
        let file = FileId::from_raw(1);
        open_docs.open(file);

        let store = JavaParseStore::new(&manager, open_docs);
        let db = Database::new_with_memory_manager(&manager);
        db.set_java_parse_store(Some(store));

        let text1 = Arc::new("class Foo {}".to_string());
        db.set_file_exists(file, true);
        db.set_file_content(file, text1.clone());

        let parse1 = db.snapshot().parse_java(file);
        assert!(parse1.errors.is_empty());
        assert_eq!(
            u32::from(parse1.syntax().text_range().end()) as usize,
            text1.len()
        );

        // Pinned parse_java should survive memo eviction while the text identity matches.
        db.evict_salsa_memos(MemoryPressure::Critical);
        let parse1_after_evict = db.snapshot().parse_java(file);
        assert!(
            Arc::ptr_eq(&parse1, &parse1_after_evict),
            "expected open document parse_java to be reused across eviction when text is unchanged"
        );

        // Changing the file text should invalidate the pinned parse_java (even if the file is still
        // open) to avoid returning stale trees.
        let text2 = Arc::new("class Foo { int x; }".to_string());
        db.set_file_content(file, text2.clone());
        let parse2 = db.snapshot().parse_java(file);
        assert!(
            !Arc::ptr_eq(&parse1, &parse2),
            "expected parse_java to recompute after file_content changes"
        );
        assert!(parse2.errors.is_empty());
        assert_eq!(
            u32::from(parse2.syntax().text_range().end()) as usize,
            text2.len()
        );

        // The new parse_java should now be pinned and survive a subsequent eviction.
        db.evict_salsa_memos(MemoryPressure::Critical);
        let parse2_after_evict = db.snapshot().parse_java(file);
        assert!(
            Arc::ptr_eq(&parse2, &parse2_after_evict),
            "expected new open document parse_java to be reused across eviction"
        );
    }

    #[test]
    fn unpin_syntax_tree_restores_salsa_memo_accounting() {
        let manager = MemoryManager::new(MemoryBudget::from_total(10 * 1024 * 1024));
        let open_docs = Arc::new(OpenDocuments::default());
        let store = SyntaxTreeStore::new(&manager, open_docs.clone());

        let db = Database::new_with_memory_manager(&manager);
        db.set_syntax_tree_store(store);

        let file = FileId::from_raw(1);
        open_docs.open(file);

        let text = "class Foo { int x; }\n".repeat(128);
        let text_len = text.len() as u64;
        db.set_file_text(file, text);

        // Parse once while open: the result should be pinned and *not* counted as a Salsa memo.
        let parse_before = db.snapshot().parse(file);
        assert!(parse_before.errors.is_empty());

        let report_open = manager.report();
        assert!(
            report_open.usage.syntax_trees > 0,
            "expected pinned parse to be tracked under syntax_trees"
        );
        assert!(
            report_open.usage.query_cache < text_len / 2,
            "expected query_cache usage to suppress pinned parse memo bytes (query_cache={}, text_len={text_len})",
            report_open.usage.query_cache
        );

        // Simulate closing the document and unpinning: the pinned tree should be removed and
        // memo accounting should move back to the Salsa query cache category.
        open_docs.close(file);
        db.unpin_syntax_tree(file);

        let report_closed = manager.report();
        assert!(
            report_closed.usage.syntax_trees < text_len / 2,
            "expected syntax_trees usage to drop after unpin (syntax_trees={}, text_len={text_len})",
            report_closed.usage.syntax_trees
        );
        assert!(
            report_closed.usage.query_cache >= text_len,
            "expected query_cache usage to restore parse memo bytes after unpin (query_cache={}, text_len={text_len})",
            report_closed.usage.query_cache
        );
    }

    #[test]
    fn unpin_item_tree_restores_salsa_memo_accounting() {
        let manager = MemoryManager::new(MemoryBudget::from_total(10 * 1024 * 1024));
        let open_docs = Arc::new(OpenDocuments::default());

        let db = Database::new_with_memory_manager(&manager);
        db.attach_item_tree_store(&manager, open_docs.clone());

        let file = FileId::from_raw(1);
        open_docs.open(file);

        let text = "class Foo { int x; }\n".repeat(128);
        let text_len = text.len() as u64;
        db.set_file_text(file, text);

        // Compute the item_tree once while open. The item_tree itself should be pinned and *not*
        // counted as a Salsa memo (only its dependency `parse` should be).
        let it = db.snapshot().item_tree(file);
        assert!(!it.items.is_empty(), "expected item_tree to contain items");

        let report_open = manager.report();
        assert!(
            report_open.usage.syntax_trees > 0,
            "expected pinned item_tree to be tracked under syntax_trees"
        );
        assert!(
            report_open.usage.query_cache < text_len.saturating_mul(3) / 2,
            "expected query_cache usage to not include pinned item_tree memo bytes (query_cache={}, text_len={text_len})",
            report_open.usage.query_cache
        );

        // Simulate closing the document and unpinning: the pinned item_tree should be removed and
        // memo accounting should move back to the Salsa query cache category.
        open_docs.close(file);
        db.unpin_item_tree(file);

        let report_closed = manager.report();
        assert!(
            report_closed.usage.syntax_trees < text_len / 2,
            "expected syntax_trees usage to drop after unpin (syntax_trees={}, text_len={text_len})",
            report_closed.usage.syntax_trees
        );
        assert!(
            report_closed.usage.query_cache >= text_len.saturating_mul(2),
            "expected query_cache usage to restore item_tree memo bytes after unpin (query_cache={}, text_len={text_len})",
            report_closed.usage.query_cache
        );
    }

    #[test]
    fn open_document_reuses_parse_after_memory_manager_enforce() {
        // Ensure `MemoryManager::enforce()` evicts Salsa memos (query cache) while leaving the
        // `SyntaxTreeStore` intact so open documents can reuse pinned parse results.
        //
        // We force eviction by setting an intentionally tiny query-cache budget, while keeping
        // the overall total (and syntax tree) budgets very large so pressure stays low and the
        // syntax tree store is not itself evicted.
        let total = 1_000_000_000_000_u64;
        let manager = MemoryManager::new(MemoryBudget {
            total,
            categories: nova_memory::MemoryBreakdown {
                query_cache: 1,
                syntax_trees: total / 2,
                indexes: 0,
                type_info: 0,
                other: total - (total / 2) - 1,
            },
        });

        let open_docs = Arc::new(OpenDocuments::default());
        let store = SyntaxTreeStore::new(&manager, open_docs.clone());

        let db = Database::new_with_memory_manager(&manager);
        db.set_syntax_tree_store(store);

        let file = FileId::from_raw(1);
        open_docs.open(file);

        db.set_file_text(file, "class Foo { int x; }");

        let before = db.with_snapshot(|snap| {
            let parse = snap.parse(file);
            // Force non-zero query-cache usage so enforcement triggers Salsa memo eviction.
            let _ = snap.parse_java(file);
            parse
        });
        assert!(
            db.salsa_memo_bytes() > 0,
            "expected memo tracker to be non-zero prior to enforcement"
        );

        manager.enforce();
        assert_eq!(
            db.salsa_memo_bytes(),
            0,
            "expected memo tracker to clear after enforcement-driven eviction"
        );

        let after = db.snapshot().parse(file);
        assert!(
            Arc::ptr_eq(&before, &after),
            "expected open document parse to be reused after enforcement-driven memo eviction"
        );
    }

    #[test]
    fn open_document_reuses_item_tree_after_memory_manager_enforce() {
        // Ensure `MemoryManager::enforce()` evicts Salsa memos (query cache) while leaving the
        // `ItemTreeStore` intact so open documents can reuse pinned item_tree results.
        let total = 1_000_000_000_000_u64;
        let manager = MemoryManager::new(MemoryBudget {
            total,
            categories: nova_memory::MemoryBreakdown {
                query_cache: 1,
                syntax_trees: total / 2,
                indexes: 0,
                type_info: 0,
                other: total - (total / 2) - 1,
            },
        });

        let open_docs = Arc::new(OpenDocuments::default());
        let db = Database::new_with_memory_manager(&manager);
        db.attach_item_tree_store(&manager, open_docs.clone());

        let file = FileId::from_raw(1);
        open_docs.open(file);
        db.set_file_text(file, "class Foo { int x; }");

        let before = db.snapshot().item_tree(file);
        assert!(
            !before.items.is_empty(),
            "expected item_tree to contain items for open document"
        );
        assert!(
            db.salsa_memo_bytes() > 0,
            "expected memo tracker to be non-zero prior to enforcement"
        );

        manager.enforce();
        assert_eq!(
            db.salsa_memo_bytes(),
            0,
            "expected memo tracker to clear after enforcement-driven eviction"
        );

        let after = db.snapshot().item_tree(file);
        assert!(
            Arc::ptr_eq(&before, &after),
            "expected open document item_tree to be reused after enforcement-driven memo eviction"
        );
    }

    #[test]
    fn open_document_reuses_parse_java_after_memory_manager_enforce() {
        use nova_syntax::JavaParseStore;
        use nova_vfs::OpenDocuments;

        // Ensure `MemoryManager::enforce()` evicts Salsa memos (query cache) while leaving the
        // `JavaParseStore` intact so open documents can reuse pinned parse_java results.
        let total = 1_000_000_000_000_u64;
        let manager = MemoryManager::new(MemoryBudget {
            total,
            categories: nova_memory::MemoryBreakdown {
                query_cache: 1,
                syntax_trees: total / 2,
                indexes: 0,
                type_info: 0,
                other: total - (total / 2) - 1,
            },
        });

        let open_docs = Arc::new(OpenDocuments::default());
        let store = JavaParseStore::new(&manager, open_docs.clone());

        let db = Database::new_with_memory_manager(&manager);
        db.set_java_parse_store(Some(store));

        let file = FileId::from_raw(1);
        open_docs.open(file);
        db.set_file_text(file, "class Foo { int x; }");

        let before = db.with_snapshot(|snap| {
            let parse_java = snap.parse_java(file);
            // Force non-zero query-cache usage so enforcement triggers Salsa memo eviction.
            let _ = snap.parse(file);
            parse_java
        });
        assert!(
            before.errors.is_empty(),
            "expected initial parse_java to succeed"
        );
        assert!(
            db.salsa_memo_bytes() > 0,
            "expected memo tracker to be non-zero prior to enforcement"
        );

        manager.enforce();
        assert_eq!(
            db.salsa_memo_bytes(),
            0,
            "expected memo tracker to clear after enforcement-driven eviction"
        );

        let after = db.snapshot().parse_java(file);
        assert!(
            Arc::ptr_eq(&before, &after),
            "expected open document parse_java to be reused after enforcement-driven memo eviction"
        );
    }

    #[test]
    fn closed_document_does_not_reuse_item_tree_after_salsa_memo_eviction() {
        let manager = MemoryManager::new(MemoryBudget::from_total(1_000_000));
        let open_docs = Arc::new(OpenDocuments::default());

        let db = Database::new_with_memory_manager(&manager);
        db.attach_item_tree_store(&manager, open_docs);

        let file = FileId::from_raw(1);
        let text = Arc::new("class Foo {}".to_string());
        db.set_file_exists(file, true);
        db.set_file_content(file, text);

        let before = db.snapshot().item_tree(file);
        db.evict_salsa_memos(MemoryPressure::Critical);
        let after = db.snapshot().item_tree(file);

        assert!(
            !Arc::ptr_eq(&before, &after),
            "expected closed document item_tree to be recomputed after memo eviction"
        );
    }

    #[test]
    fn closed_document_does_not_reuse_parse_after_salsa_memo_eviction() {
        let manager = MemoryManager::new(MemoryBudget::from_total(1_000_000));
        let open_docs = Arc::new(OpenDocuments::default());
        let file = FileId::from_raw(1);

        let store = SyntaxTreeStore::new(&manager, open_docs);
        let db = Database::new_with_memory_manager(&manager);
        db.set_syntax_tree_store(store);

        let text = Arc::new("class Foo { int x; }".to_string());
        db.set_file_exists(file, true);
        db.set_file_content(file, text);

        let before = db.snapshot().parse(file);
        db.evict_salsa_memos(MemoryPressure::Critical);
        let after = db.snapshot().parse(file);

        assert!(
            !Arc::ptr_eq(&before, &after),
            "expected closed document parse to be recomputed after memo eviction"
        );
    }

    #[test]
    fn java_parse_cache_clears_on_salsa_memo_eviction() {
        let db = Database::new();
        let file = FileId::from_raw(1);

        db.set_file_text(file, "class Foo { int x; }");
        db.with_snapshot(|snap| {
            let parse = snap.parse_java(file);
            assert!(
                parse.errors.is_empty(),
                "expected initial Java parse to succeed, got: {:?}",
                parse.errors
            );
        });

        assert!(
            db.inner.lock().java_parse_cache.entry_count() > 0,
            "expected incremental parse cache to be populated after parse_java"
        );

        db.evict_salsa_memos(MemoryPressure::Critical);
        assert_eq!(
            db.inner.lock().java_parse_cache.entry_count(),
            0,
            "expected incremental parse cache to clear after memo eviction"
        );

        // Subsequent edit should fall back to a full parse (cache miss) without panicking.
        db.set_file_text(file, "class Foo { int x; int y; }");
        db.with_snapshot(|snap| {
            let parse = snap.parse_java(file);
            assert!(
                parse.errors.is_empty(),
                "expected Java parse after eviction to succeed, got: {:?}",
                parse.errors
            );
        });

        assert!(
            db.inner.lock().java_parse_cache.entry_count() > 0,
            "expected incremental parse cache to be repopulated after reparse"
        );
    }

    #[test]
    fn java_parse_cache_clears_on_memory_manager_eviction() {
        // Ensure that Nova's MemoryManager eviction path (via `SalsaMemoEvictor`) clears the
        // incremental Java parse cache so it can't retain parse trees after Salsa memos are
        // dropped.
        let manager = MemoryManager::new(MemoryBudget::from_total(1_000));
        let db = Database::new_with_memory_manager(&manager);

        let file = FileId::from_raw(1);
        // Ensure `TrackedSalsaMemo::ParseJava` exceeds the query cache budget so eviction runs.
        let text = "class Foo { int x; }\n".repeat(256);
        db.set_file_text(file, text);
        db.with_snapshot(|snap| {
            let _ = snap.parse_java(file);
        });

        assert!(
            db.inner.lock().java_parse_cache.entry_count() > 0,
            "expected incremental parse cache to be populated after parse_java"
        );

        manager.enforce();

        assert_eq!(
            db.inner.lock().java_parse_cache.entry_count(),
            0,
            "expected incremental parse cache to clear after MemoryManager eviction"
        );
    }

    #[test]
    fn java_parse_cache_enforces_lru_entry_cap() {
        // Keep in sync with `java_parse_cache::DEFAULT_ENTRY_CAP`.
        const CAP: u32 = 64;

        let db = Database::new();
        for idx in 0..CAP {
            let file = FileId::from_raw(idx + 1);
            db.set_file_text(file, format!("class C{idx} {{ int x = {idx}; }}"));
            db.with_snapshot(|snap| {
                let _ = snap.parse_java(file);
            });
        }

        let cache = db.inner.lock().java_parse_cache.clone();
        assert_eq!(
            cache.entry_count() as u32,
            CAP,
            "expected java_parse_cache to contain exactly CAP entries after initial population"
        );

        // Touch the first entry to make it most-recent so the second entry becomes LRU.
        let first = FileId::from_raw(1);
        assert!(
            cache.get(first).is_some(),
            "expected first entry to exist in the cache"
        );

        let extra = FileId::from_raw(CAP + 1);
        db.set_file_text(extra, "class Extra { int y; }");
        db.with_snapshot(|snap| {
            let _ = snap.parse_java(extra);
        });

        assert_eq!(
            cache.entry_count() as u32,
            CAP,
            "expected java_parse_cache to keep a stable CAP after inserting an extra entry"
        );
        assert!(
            cache.get(FileId::from_raw(2)).is_none(),
            "expected the least-recently-used entry (file 2) to be evicted"
        );
        assert!(
            cache.get(first).is_some(),
            "expected the recently-used entry (file 1) to remain in the cache"
        );
    }

    #[test]
    fn parse_java_results_are_pinned_for_open_documents_across_memo_eviction() {
        use nova_syntax::JavaParseStore;
        use nova_vfs::OpenDocuments;

        let manager = MemoryManager::new(MemoryBudget::from_total(1024 * 1024));
        let db = Database::new_with_memory_manager(&manager);

        let open_docs = Arc::new(OpenDocuments::default());
        let store = JavaParseStore::new(&manager, open_docs.clone());
        db.set_java_parse_store(Some(store));

        let open_file = FileId::from_raw(1);
        let closed_file = FileId::from_raw(2);

        let open_text = Arc::new("class Open {}".to_string());
        let closed_text = Arc::new("class Closed {}".to_string());

        db.set_file_exists(open_file, true);
        db.set_file_content(open_file, Arc::clone(&open_text));
        db.set_file_exists(closed_file, true);
        db.set_file_content(closed_file, Arc::clone(&closed_text));

        // Both files are initially open, so both results are inserted.
        open_docs.open(open_file);
        open_docs.open(closed_file);
        let (open_before, closed_before) =
            db.with_snapshot(|snap| (snap.parse_java(open_file), snap.parse_java(closed_file)));

        // Close one file, then evict Salsa memos. Only the open file should reuse
        // its parse tree from the open-document store.
        open_docs.close(closed_file);
        db.evict_salsa_memos(MemoryPressure::Critical);

        let (open_after, closed_after) =
            db.with_snapshot(|snap| (snap.parse_java(open_file), snap.parse_java(closed_file)));

        assert!(Arc::ptr_eq(&open_before, &open_after));
        assert!(!Arc::ptr_eq(&closed_before, &closed_after));
    }

    #[test]
    fn persistence_mode_does_not_change_query_results() {
        let tmp = TempDir::new().unwrap();
        let project_root = tmp.path().join("project");
        std::fs::create_dir_all(&project_root).unwrap();

        let cache_root = tmp.path().join("cache");
        std::fs::create_dir_all(&cache_root).unwrap();

        let cache_cfg = CacheConfig {
            cache_root_override: Some(cache_root),
        };

        let file = FileId::from_raw(1);
        let file_path = "src/Foo.java";
        let text = Arc::new("class Foo { int x; }".to_string());

        // First run: RW (populates cache).
        let mut rw_db = RootDatabase::new_with_persistence(
            &project_root,
            PersistenceConfig {
                mode: crate::PersistenceMode::ReadWrite,
                cache: cache_cfg.clone(),
            },
        );
        rw_db.set_file_path(file, file_path);
        rw_db.set_file_text_full(file, text.clone());

        let from_rw = rw_db.item_tree(file);
        drop(rw_db);

        // Second run: RW again (should be able to warm-start from disk).
        let mut rw_db2 = RootDatabase::new_with_persistence(
            &project_root,
            PersistenceConfig {
                mode: crate::PersistenceMode::ReadWrite,
                cache: cache_cfg.clone(),
            },
        );
        rw_db2.set_file_path(file, file_path);
        rw_db2.set_file_text_full(file, text.clone());

        let from_cache = rw_db2.item_tree(file);
        assert_eq!(&*from_cache, &*from_rw);
        assert_eq!(executions(&rw_db2, "parse"), 0);
        assert_eq!(stat(&rw_db2, "item_tree").disk_hits, 1);
        assert_eq!(stat(&rw_db2, "item_tree").disk_misses, 0);

        // Third run: disabled (must ignore cache but produce identical results).
        let mut disabled_db = RootDatabase::new_with_persistence(
            &project_root,
            PersistenceConfig {
                mode: crate::PersistenceMode::Disabled,
                cache: cache_cfg,
            },
        );
        disabled_db.set_file_path(file, file_path);
        disabled_db.set_file_text_full(file, text);

        let from_disabled = disabled_db.item_tree(file);
        assert_eq!(&*from_disabled, &*from_rw);
        assert_eq!(stat(&disabled_db, "item_tree").disk_hits, 0);
        assert_eq!(stat(&disabled_db, "item_tree").disk_misses, 0);
    }

    #[test]
    fn dirty_files_do_not_overwrite_persisted_ast_artifacts() {
        let tmp = TempDir::new().unwrap();
        let project_root = tmp.path().join("project");
        std::fs::create_dir_all(project_root.join("src")).unwrap();

        let cache_root = tmp.path().join("cache");
        std::fs::create_dir_all(&cache_root).unwrap();

        let cache_cfg = CacheConfig {
            cache_root_override: Some(cache_root),
        };

        let rel_path = "src/A.java";
        let disk_text = "class A {}";
        std::fs::write(project_root.join(rel_path), disk_text).unwrap();

        let file = FileId::from_raw(1);

        // First run: parse + persist artifacts for the on-disk content.
        let mut db = RootDatabase::new_with_persistence(
            &project_root,
            PersistenceConfig {
                mode: crate::PersistenceMode::ReadWrite,
                cache: cache_cfg.clone(),
            },
        );
        db.set_file_path(file, rel_path);
        db.set_file_text(file, disk_text);
        let _ = db.item_tree(file);

        // Second run in the same DB: mutate in-memory only (dirty overlay) and ensure we do *not*
        // overwrite the persisted artifacts.
        db.set_file_text(file, "class A { int x; }");
        db.set_file_is_dirty(file, true);
        let _ = db.item_tree(file);
        drop(db);

        // Third run (fresh DB): original disk content should still warm-start.
        let mut db = RootDatabase::new_with_persistence(
            &project_root,
            PersistenceConfig {
                mode: crate::PersistenceMode::ReadWrite,
                cache: cache_cfg,
            },
        );
        db.set_file_path(file, rel_path);
        db.set_file_text(file, disk_text);

        db.clear_query_stats();
        let _ = db.item_tree(file);

        assert_eq!(
            executions(&db, "parse"),
            0,
            "expected warm-start from persisted artifacts (parse should not execute)"
        );
        assert_eq!(stat(&db, "item_tree").disk_hits, 1);
        assert_eq!(stat(&db, "item_tree").disk_misses, 0);
    }

    #[test]
    fn read_only_mode_does_not_write_cache() {
        let tmp = TempDir::new().unwrap();
        let project_root = tmp.path().join("project");
        std::fs::create_dir_all(&project_root).unwrap();

        let cache_root = tmp.path().join("cache");
        std::fs::create_dir_all(&cache_root).unwrap();

        let cache_cfg = CacheConfig {
            cache_root_override: Some(cache_root.clone()),
        };

        let file = FileId::from_raw(1);
        let rel_path = "src/Foo.java";
        let text = Arc::new("class Foo { int x; }".to_string());

        // Read-only mode should allow reads but never write back.
        let mut ro_db = RootDatabase::new_with_persistence(
            &project_root,
            PersistenceConfig {
                mode: crate::PersistenceMode::ReadOnly,
                cache: cache_cfg.clone(),
            },
        );
        ro_db.set_file_path(file, rel_path);
        ro_db.set_file_text_full(file, text.clone());
        let ro_tree = ro_db.item_tree(file);
        assert_eq!(stat(&ro_db, "item_tree").disk_hits, 0);
        assert_eq!(stat(&ro_db, "item_tree").disk_misses, 1);
        assert_eq!(
            ro_db.persistence_stats().ast_store_success,
            0,
            "read-only mode must not write AST artifacts"
        );
        drop(ro_db);

        let cache_dir = nova_cache::CacheDir::new(&project_root, cache_cfg.clone()).unwrap();
        let artifact_name = format!(
            "{}.ast",
            nova_cache::Fingerprint::from_bytes(rel_path.as_bytes()).as_str()
        );
        let artifact_path = cache_dir.ast_dir().join(artifact_name);
        assert!(
            !artifact_path.exists(),
            "read-only mode must not create cache artifacts"
        );

        // A subsequent read-write run should not be able to warm-start.
        let mut rw_db = RootDatabase::new_with_persistence(
            &project_root,
            PersistenceConfig {
                mode: crate::PersistenceMode::ReadWrite,
                cache: cache_cfg,
            },
        );
        rw_db.set_file_path(file, rel_path);
        rw_db.set_file_text_full(file, text);
        let rw_tree = rw_db.item_tree(file);
        assert_eq!(&*rw_tree, &*ro_tree);
        assert_eq!(stat(&rw_db, "item_tree").disk_hits, 0);
        assert_eq!(stat(&rw_db, "item_tree").disk_misses, 1);
    }

    #[test]
    fn corrupted_cache_is_ignored_and_forces_recompute() {
        let tmp = TempDir::new().unwrap();
        let project_root = tmp.path().join("project");
        std::fs::create_dir_all(&project_root).unwrap();

        let cache_root = tmp.path().join("cache");
        std::fs::create_dir_all(&cache_root).unwrap();

        let cache_cfg = CacheConfig {
            cache_root_override: Some(cache_root.clone()),
        };

        let file = FileId::from_raw(1);
        let rel_path = "src/Foo.java";
        let text = Arc::new("class Foo { int x; }".to_string());

        // Populate cache.
        let mut db = RootDatabase::new_with_persistence(
            &project_root,
            PersistenceConfig {
                mode: crate::PersistenceMode::ReadWrite,
                cache: cache_cfg.clone(),
            },
        );
        db.set_file_path(file, rel_path);
        db.set_file_text_full(file, text.clone());
        let expected = db.item_tree(file);
        drop(db);

        // Corrupt the artifact file.
        let cache_dir = nova_cache::CacheDir::new(&project_root, cache_cfg).unwrap();
        let ast_dir = cache_dir.ast_dir();
        let artifact_name = format!(
            "{}.ast",
            nova_cache::Fingerprint::from_bytes(rel_path.as_bytes()).as_str()
        );
        let artifact_path = ast_dir.join(artifact_name);
        assert!(artifact_path.exists(), "expected cache artifact to exist");
        std::fs::write(&artifact_path, b"corrupted").unwrap();

        // New DB should treat corruption as cache miss and recompute.
        let mut db2 = RootDatabase::new_with_persistence(
            &project_root,
            PersistenceConfig {
                mode: crate::PersistenceMode::ReadWrite,
                cache: CacheConfig {
                    cache_root_override: Some(cache_root),
                },
            },
        );
        db2.set_file_path(file, rel_path);
        db2.set_file_text_full(file, text);

        let actual = db2.item_tree(file);
        assert_eq!(&*actual, &*expected);
        assert_eq!(stat(&db2, "item_tree").disk_hits, 0);
        assert_eq!(stat(&db2, "item_tree").disk_misses, 1);

        let stats = db2.persistence_stats();
        assert!(
            stats.ast_load_misses > 0 && stats.ast_store_success > 0,
            "expected corruption to be treated as miss + recompute: {stats:?}"
        );
    }

    #[test]
    fn persistent_derived_query_roundtrip_and_invalidation() {
        ide::UPPERCASED_FILE_WORDS_COMPUTE_COUNT.store(0, Ordering::SeqCst);

        let tmp = TempDir::new().unwrap();
        let project_root = tmp.path().join("project");
        std::fs::create_dir_all(&project_root).unwrap();

        let cache_root = tmp.path().join("cache");
        std::fs::create_dir_all(&cache_root).unwrap();

        let cache_cfg = CacheConfig {
            cache_root_override: Some(cache_root),
        };

        let file = FileId::from_raw(1);
        let file_path = "src/Foo.java";

        // First run: compute + persist.
        {
            let mut db = RootDatabase::new_with_persistence(
                &project_root,
                PersistenceConfig {
                    mode: crate::PersistenceMode::ReadWrite,
                    cache: cache_cfg.clone(),
                },
            );
            db.set_file_path(file, file_path);
            db.set_file_text(file, "hello world");

            let words = db.uppercased_file_words(file);
            assert_eq!(words, vec!["HELLO".to_string(), "WORLD".to_string()]);
            assert_eq!(
                ide::UPPERCASED_FILE_WORDS_COMPUTE_COUNT.load(Ordering::SeqCst),
                1
            );
        }

        // Second run: same inputs should load without recomputing.
        let mut db = RootDatabase::new_with_persistence(
            &project_root,
            PersistenceConfig {
                mode: crate::PersistenceMode::ReadWrite,
                cache: cache_cfg.clone(),
            },
        );
        db.set_file_path(file, file_path);
        db.set_file_text(file, "hello world");

        let words = db.uppercased_file_words(file);
        assert_eq!(words, vec!["HELLO".to_string(), "WORLD".to_string()]);
        assert_eq!(
            ide::UPPERCASED_FILE_WORDS_COMPUTE_COUNT.load(Ordering::SeqCst),
            1,
            "expected persistent derived cache hit"
        );

        // Input change: should invalidate and recompute.
        db.set_file_text(file, "hello nova");
        let words = db.uppercased_file_words(file);
        assert_eq!(words, vec!["HELLO".to_string(), "NOVA".to_string()]);
        assert_eq!(
            ide::UPPERCASED_FILE_WORDS_COMPUTE_COUNT.load(Ordering::SeqCst),
            2
        );
    }

    #[test]
    fn persistence_derived_query_schema_version_is_cache_miss() {
        let tmp = TempDir::new().unwrap();
        let project_root = tmp.path().join("project");
        std::fs::create_dir_all(&project_root).unwrap();

        let cache_root = tmp.path().join("cache");
        std::fs::create_dir_all(&cache_root).unwrap();

        let persistence = Persistence::new(
            &project_root,
            PersistenceConfig {
                mode: crate::PersistenceMode::ReadWrite,
                cache: CacheConfig {
                    cache_root_override: Some(cache_root),
                },
            },
        );

        let args = (1_u32,);
        let mut inputs = BTreeMap::new();
        inputs.insert("file_content".to_string(), Fingerprint::from_bytes("v1"));

        let calls = std::sync::atomic::AtomicUsize::new(0);

        let first: u32 = persistence.get_or_compute_derived("demo", 1, &args, &inputs, || {
            calls.fetch_add(1, Ordering::SeqCst);
            42
        });
        assert_eq!(first, 42);

        let second: u32 = persistence.get_or_compute_derived("demo", 1, &args, &inputs, || {
            calls.fetch_add(1, Ordering::SeqCst);
            43
        });
        assert_eq!(second, 42, "same schema version should hit");

        let third: u32 = persistence.get_or_compute_derived("demo", 2, &args, &inputs, || {
            calls.fetch_add(1, Ordering::SeqCst);
            44
        });
        assert_eq!(third, 44, "new schema version should miss and recompute");

        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
