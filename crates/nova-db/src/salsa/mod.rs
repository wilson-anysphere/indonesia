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
mod hir;
mod ide;
mod indexing;
mod inputs;
mod resolve;
mod semantic;
mod stats;
mod syntax;

pub use hir::NovaHir;
pub use ide::NovaIde;
pub use indexing::NovaIndexing;
pub use inputs::NovaInputs;
pub use resolve::NovaResolve;
pub use semantic::NovaSemantic;
pub use stats::{HasQueryStats, QueryStat, QueryStatReport, QueryStats, QueryStatsReport};
pub use syntax::{NovaSyntax, SyntaxTree};

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use parking_lot::Mutex as ParkingMutex;
use parking_lot::RwLock;

use nova_memory::{
    EvictionRequest, EvictionResult, MemoryCategory, MemoryEvictor, MemoryManager, MemoryPressure,
};
use nova_project::{BuildSystem, JavaConfig, JavaVersion, ProjectConfig};

use crate::persistence::{HasPersistence, Persistence, PersistenceConfig};
use crate::{FileId, ProjectId, SourceRootId};

use self::stats::QueryStatsCollector;

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

/// File-keyed memoized query results tracked for memory accounting.
///
/// This is intentionally coarse: it is used to approximate the footprint of
/// Salsa memo tables which can otherwise grow without bound in large workspaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrackedSalsaMemo {
    Parse,
    ParseJava,
    ItemTree,
}

/// Database functionality needed by query implementations to record memo sizes.
///
/// Implementations should treat the values as best-effort hints and must not
/// panic if accounting fails.
pub trait HasSalsaMemoStats {
    fn record_salsa_memo_bytes(&self, file: FileId, memo: TrackedSalsaMemo, bytes: u64);
}

#[derive(Debug, Default)]
struct SalsaMemoFootprint {
    inner: Mutex<SalsaMemoFootprintInner>,
    tracker: OnceLock<nova_memory::MemoryTracker>,
}

#[derive(Debug, Default)]
struct SalsaMemoFootprintInner {
    by_file: HashMap<FileId, FileMemoBytes>,
    total_bytes: u64,
}

#[derive(Debug, Clone, Copy, Default)]
struct FileMemoBytes {
    parse: u64,
    parse_java: u64,
    item_tree: u64,
}

impl FileMemoBytes {
    fn total(self) -> u64 {
        self.parse + self.parse_java + self.item_tree
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
        inner.total_bytes = 0;
        drop(inner);
        self.refresh_tracker();
    }

    fn record(&self, file: FileId, memo: TrackedSalsaMemo, bytes: u64) {
        let mut inner = self.lock_inner();
        let entry = inner.by_file.entry(file).or_default();
        let prev_total = entry.total();

        match memo {
            TrackedSalsaMemo::Parse => entry.parse = bytes,
            TrackedSalsaMemo::ParseJava => entry.parse_java = bytes,
            TrackedSalsaMemo::ItemTree => entry.item_tree = bytes,
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

#[derive(Debug, Default, Clone)]
struct SalsaInputs {
    file_exists: HashMap<FileId, bool>,
    file_project: HashMap<FileId, ProjectId>,
    file_content: HashMap<FileId, Arc<String>>,
    file_rel_path: HashMap<FileId, Arc<String>>,
    source_root: HashMap<FileId, SourceRootId>,
    project_files: HashMap<ProjectId, Arc<Vec<FileId>>>,
    project_config: HashMap<ProjectId, Arc<ProjectConfig>>,
    jdk_index: HashMap<ProjectId, ArcEq<nova_jdk::JdkIndex>>,
    classpath_index: HashMap<ProjectId, Option<ArcEq<nova_classpath::ClasspathIndex>>>,
}

impl SalsaInputs {
    fn apply_to(&self, db: &mut RootDatabase) {
        for (&file, &exists) in &self.file_exists {
            db.set_file_exists(file, exists);
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
        for (&file, path) in &self.file_rel_path {
            db.set_file_rel_path(file, path.clone());
        }
        for (&project, files) in &self.project_files {
            db.set_project_files(project, files.clone());
        }
        for (&project, config) in &self.project_config {
            db.set_project_config(project, config.clone());
        }
        for (&project, index) in &self.jdk_index {
            db.set_jdk_index(project, index.clone());
        }
        for (&project, index) in &self.classpath_index {
            db.set_classpath_index(project, index.clone());
        }
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
    hir::NovaHirStorage,
    resolve::NovaResolveStorage,
    ide::NovaIdeStorage,
    indexing::NovaIndexingStorage
)]
pub struct RootDatabase {
    storage: ra_salsa::Storage<RootDatabase>,
    stats: QueryStatsCollector,
    persistence: Persistence,
    file_paths: Arc<RwLock<HashMap<FileId, Arc<String>>>>,
    memo_footprint: Arc<SalsaMemoFootprint>,
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
            memo_footprint: Arc::new(SalsaMemoFootprint::default()),
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

        db
    }

    pub fn set_file_path(&mut self, file: FileId, path: impl Into<String>) {
        self.file_paths.write().insert(file, Arc::new(path.into()));
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
            memo_footprint: self.memo_footprint.clone(),
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

    fn evict(&self, request: EvictionRequest) -> EvictionResult {
        let before = self.footprint.bytes();
        if before <= request.target_bytes {
            return EvictionResult {
                before_bytes: before,
                after_bytes: before,
            };
        }

        // Eviction must be best-effort and non-panicking. `ra_ap_salsa` does not
        // currently expose a stable per-query sweep API, so we rebuild the
        // database from inputs and swap it behind the mutex. Outstanding
        // snapshots remain valid because they own their storage snapshots.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let inputs = self.inputs.lock().clone();
            let mut db = self.db.lock();
            let stats = db.stats.clone();
            let persistence = db.persistence.clone();
            let file_paths = db.file_paths.clone();
            let mut fresh = RootDatabase {
                storage: ra_salsa::Storage::default(),
                stats,
                persistence,
                file_paths,
                memo_footprint: self.footprint.clone(),
            };
            inputs.apply_to(&mut fresh);
            *db = fresh;
        }));

        // Clear tracked footprint unconditionally; memos will be re-recorded as
        // queries re-execute.
        self.footprint.clear();
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
    memo_footprint: Arc<SalsaMemoFootprint>,
}

impl Default for Database {
    fn default() -> Self {
        let db = RootDatabase::default();
        let memo_footprint = db.memo_footprint.clone();
        let mut inputs = SalsaInputs::default();
        let default_project = ProjectId::from_raw(0);
        inputs
            .project_config
            .insert(default_project, db.project_config(default_project));
        Self {
            inner: Arc::new(ParkingMutex::new(db)),
            inputs: Arc::new(ParkingMutex::new(inputs)),
            memo_evictor: Arc::new(OnceLock::new()),
            memo_footprint,
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
        db
    }

    pub fn new_with_persistence(
        project_root: impl AsRef<Path>,
        persistence: PersistenceConfig,
    ) -> Self {
        let db = RootDatabase::new_with_persistence(project_root, persistence);
        let memo_footprint = db.memo_footprint.clone();
        let mut inputs = SalsaInputs::default();
        let default_project = ProjectId::from_raw(0);
        inputs
            .project_config
            .insert(default_project, db.project_config(default_project));
        Self {
            inner: Arc::new(ParkingMutex::new(db)),
            inputs: Arc::new(ParkingMutex::new(inputs)),
            memo_evictor: Arc::new(OnceLock::new()),
            memo_footprint,
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.inner.lock().snapshot()
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

    pub fn with_write<T>(&self, f: impl FnOnce(&mut RootDatabase) -> T) -> T {
        let mut db = self.inner.lock();
        f(&mut db)
    }

    pub fn request_cancellation(&self) {
        self.inner.lock().request_cancellation();
    }

    pub fn set_file_exists(&self, file: FileId, exists: bool) {
        self.inputs.lock().file_exists.insert(file, exists);
        self.inner.lock().set_file_exists(file, exists);
    }

    pub fn set_file_content(&self, file: FileId, content: Arc<String>) {
        self.inputs
            .lock()
            .file_content
            .insert(file, content.clone());
        self.inner.lock().set_file_content(file, content);
    }

    pub fn set_file_text(&self, file: FileId, text: impl Into<String>) {
        let text = Arc::new(text.into());
        {
            let mut inputs = self.inputs.lock();
            inputs.file_exists.insert(file, true);
            inputs.file_project.insert(file, ProjectId::from_raw(0));
            inputs.source_root.insert(file, SourceRootId::from_raw(0));
            inputs.file_content.insert(file, text.clone());
        }
        let mut db = self.inner.lock();
        db.set_file_exists(file, true);
        db.set_file_project(file, ProjectId::from_raw(0));
        db.set_source_root(file, SourceRootId::from_raw(0));
        db.set_file_content(file, text);
    }

    pub fn set_file_path(&self, file: FileId, path: impl Into<String>) {
        self.inner.lock().set_file_path(file, path);
    }

    pub fn set_project_files(&self, project: ProjectId, files: Arc<Vec<FileId>>) {
        self.inputs
            .lock()
            .project_files
            .insert(project, files.clone());
        self.inner.lock().set_project_files(project, files);
    }

    pub fn set_file_rel_path(&self, file: FileId, rel_path: Arc<String>) {
        self.inputs
            .lock()
            .file_rel_path
            .insert(file, Arc::clone(&rel_path));
        let mut db = self.inner.lock();
        db.set_file_rel_path(file, Arc::clone(&rel_path));
        // Keep the non-tracked file path map in sync so existing persistence
        // caches (AST artifacts, derived caches) can reuse the same keys.
        db.set_file_path(file, rel_path.as_ref().clone());
    }

    pub fn set_project_config(&self, project: ProjectId, config: Arc<ProjectConfig>) {
        self.inputs
            .lock()
            .project_config
            .insert(project, config.clone());
        self.inner.lock().set_project_config(project, config);
    }

    pub fn set_file_project(&self, file: FileId, project: ProjectId) {
        self.inputs.lock().file_project.insert(file, project);
        self.inner.lock().set_file_project(file, project);
    }

    pub fn set_jdk_index(&self, project: ProjectId, index: Arc<nova_jdk::JdkIndex>) {
        let index = ArcEq::new(index);
        self.inputs.lock().jdk_index.insert(project, index.clone());
        self.inner.lock().set_jdk_index(project, index);
    }

    pub fn set_classpath_index(
        &self,
        project: ProjectId,
        index: Option<Arc<nova_classpath::ClasspathIndex>>,
    ) {
        let index = index.map(ArcEq::new);
        self.inputs
            .lock()
            .classpath_index
            .insert(project, index.clone());
        self.inner.lock().set_classpath_index(project, index);
    }

    pub fn set_source_root(&self, file: FileId, root: SourceRootId) {
        self.inputs.lock().source_root.insert(file, root);
        self.inner.lock().set_source_root(file, root);
    }

    /// Best-effort drop of memoized Salsa query results.
    ///
    /// Input queries are preserved; any outstanding snapshots remain valid.
    pub fn evict_salsa_memos(&self, pressure: MemoryPressure) {
        // Under low pressure, avoid disrupting cache locality.
        if matches!(pressure, MemoryPressure::Low) {
            return;
        }

        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let inputs = self.inputs.lock().clone();
            let mut db = self.inner.lock();
            let stats = db.stats.clone();
            let persistence = db.persistence.clone();
            let file_paths = db.file_paths.clone();
            let mut fresh = RootDatabase {
                storage: ra_salsa::Storage::default(),
                stats,
                persistence,
                file_paths,
                memo_footprint: self.memo_footprint.clone(),
            };
            inputs.apply_to(&mut fresh);
            *db = fresh;
        }));
        self.memo_footprint.clear();
    }

    pub fn salsa_memo_bytes(&self) -> u64 {
        self.memo_footprint.bytes()
    }

    pub fn register_salsa_memo_evictor(&self, manager: &MemoryManager) -> Arc<SalsaMemoEvictor> {
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

        let file_fingerprints = snap.project_file_fingerprints(project);
        let indexes = snap.project_indexes(project);
        let mut indexes = (*indexes).clone();

        nova_index::save_indexes_with_fingerprints(
            cache_dir,
            file_fingerprints.as_ref(),
            &mut indexes,
        )
    }
}

/// Convenience trait alias that composes Nova's query groups.
pub trait NovaDatabase:
    NovaInputs + NovaSyntax + NovaSemantic + NovaIde + NovaHir + NovaResolve + NovaIndexing
{
}

impl<T> NovaDatabase for T where
    T: NovaInputs + NovaSyntax + NovaSemantic + NovaIde + NovaHir + NovaResolve + NovaIndexing
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
    use nova_memory::{MemoryBudget, MemoryPressure};
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

    #[test]
    fn edit_invalidates_parse() {
        let mut db = RootDatabase::default();
        let file = FileId::from_raw(1);

        db.set_file_exists(file, true);
        db.set_file_content(file, Arc::new("class Foo {}".to_string()));

        let first = db.parse(file);
        assert_eq!(executions(&db, "parse"), 1);

        // Add tokens so the parse tree changes (not just ranges).
        db.set_file_content(file, Arc::new("class Foo { int x; }".to_string()));
        let second = db.parse(file);

        assert_eq!(executions(&db, "parse"), 2);
        assert_ne!(&*first, &*second);
    }

    #[test]
    fn whitespace_edit_reparses_but_early_cutoff_downstream() {
        let mut db = RootDatabase::default();
        let file = FileId::from_raw(1);

        db.set_file_exists(file, true);
        db.set_file_content(file, Arc::new("class Foo {}".to_string()));

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
        db.set_file_content(file, Arc::new("  class Foo {}".to_string()));
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

        db.set_file_exists(file, true);
        db.set_file_content(
            file,
            Arc::new("class Foo { int x; void bar() {} }".to_string()),
        );

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

        db.set_file_exists(file, true);
        db.set_file_content(
            file,
            Arc::new("class Foo { int x; void bar() { int y = 1; } }".to_string()),
        );

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

        db.set_file_content(
            file,
            Arc::new("class Foo { int x; void bar() { int y = 1; int z = 0; } }".to_string()),
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

        db.set_file_exists(file, true);
        db.set_file_content(
            file,
            Arc::new("class Foo { int x; void bar() {} }".to_string()),
        );

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
        db.set_file_content(
            file,
            Arc::new("  class Foo { int x; void bar() {} }".to_string()),
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

        db.set_file_exists(file, true);
        db.set_file_content(file, Arc::new(source.to_string()));

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

        db.set_file_exists(file, true);
        db.set_file_content(file, Arc::new("class Foo {}".to_string()));

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
        db.set_file_exists(file, true);
        db.set_source_root(file, SourceRootId::from_raw(0));
        db.set_file_content(file, Arc::new("class Foo {}".to_string()));

        assert_query_is_cancelled(db, move |snap| snap.interruptible_work(file, 5_000_000));
    }

    #[test]
    fn request_cancellation_unwinds_synthetic_semantic_query() {
        let mut db = RootDatabase::default();
        let file = FileId::from_raw(1);
        db.set_file_exists(file, true);
        db.set_source_root(file, SourceRootId::from_raw(0));
        db.set_file_content(file, Arc::new("class Foo {}".to_string()));

        assert_query_is_cancelled(db, move |snap| {
            snap.synthetic_semantic_work(file, 5_000_000)
        });
    }

    #[test]
    fn hir_queries_hit_cancellation_checkpoint() {
        use std::sync::mpsc;
        use std::time::Duration;

        let mut db = RootDatabase::default();
        let file = FileId::from_raw(1);
        db.set_file_exists(file, true);
        db.set_file_content(file, Arc::new("class Foo { int x; }".to_string()));

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

        db.set_file_exists(file, true);
        db.set_file_content(file, Arc::new("class Foo {}".to_string()));
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
        db.set_file_content(file, Arc::new("class Foo { int x; }".to_string()));
        db.parse(file);
        let third = stat(&db, "parse");
        assert_eq!(third.executions, 2);
    }

    #[test]
    fn concurrent_reads_record_blocking() {
        use std::sync::mpsc;

        let mut db = RootDatabase::default();
        let file = FileId::from_raw(1);
        db.set_file_exists(file, true);
        db.set_source_root(file, SourceRootId::from_raw(0));
        db.set_file_content(file, Arc::new("class Foo {}".to_string()));
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
        rw_db.set_file_exists(file, true);
        rw_db.set_file_path(file, file_path);
        rw_db.set_file_content(file, text.clone());

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
        rw_db2.set_file_exists(file, true);
        rw_db2.set_file_path(file, file_path);
        rw_db2.set_file_content(file, text.clone());

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
        disabled_db.set_file_exists(file, true);
        disabled_db.set_file_path(file, file_path);
        disabled_db.set_file_content(file, text);

        let from_disabled = disabled_db.item_tree(file);
        assert_eq!(&*from_disabled, &*from_rw);
        assert_eq!(stat(&disabled_db, "item_tree").disk_hits, 0);
        assert_eq!(stat(&disabled_db, "item_tree").disk_misses, 0);
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
        ro_db.set_file_exists(file, true);
        ro_db.set_file_path(file, rel_path);
        ro_db.set_file_content(file, text.clone());
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
        rw_db.set_file_exists(file, true);
        rw_db.set_file_path(file, rel_path);
        rw_db.set_file_content(file, text);
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
        db.set_file_exists(file, true);
        db.set_file_path(file, rel_path);
        db.set_file_content(file, text.clone());
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
        db2.set_file_exists(file, true);
        db2.set_file_path(file, rel_path);
        db2.set_file_content(file, text);

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
            db.set_file_exists(file, true);
            db.set_file_path(file, file_path);
            db.set_file_content(file, Arc::new("hello world".to_string()));

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
        db.set_file_exists(file, true);
        db.set_file_path(file, file_path);
        db.set_file_content(file, Arc::new("hello world".to_string()));

        let words = db.uppercased_file_words(file);
        assert_eq!(words, vec!["HELLO".to_string(), "WORLD".to_string()]);
        assert_eq!(
            ide::UPPERCASED_FILE_WORDS_COMPUTE_COUNT.load(Ordering::SeqCst),
            1,
            "expected persistent derived cache hit"
        );

        // Input change: should invalidate and recompute.
        db.set_file_content(file, Arc::new("hello nova".to_string()));
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
