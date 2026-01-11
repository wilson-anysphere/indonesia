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
mod inputs;
mod semantic;
mod stats;
mod syntax;

pub use hir::NovaHir;
pub use ide::NovaIde;
pub use inputs::NovaInputs;
pub use semantic::NovaSemantic;
pub use stats::{HasQueryStats, QueryStat, QueryStats};
pub use syntax::{NovaSyntax, SyntaxTree};

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;

use nova_project::ProjectConfig;

use crate::persistence::{HasPersistence, Persistence, PersistenceConfig};
use crate::{FileId, ProjectId, SourceRootId};

use self::stats::QueryStatsCollector;

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
    ide::NovaIdeStorage
)]
pub struct RootDatabase {
    storage: ra_salsa::Storage<RootDatabase>,
    stats: QueryStatsCollector,
    persistence: Persistence,
    file_paths: Arc<RwLock<HashMap<FileId, Arc<String>>>>,
}

impl Default for RootDatabase {
    fn default() -> Self {
        let project_root =
            std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf());
        Self::new_with_persistence(project_root, PersistenceConfig::from_env())
    }
}

impl RootDatabase {
    pub fn new_with_persistence(
        project_root: impl AsRef<Path>,
        persistence: PersistenceConfig,
    ) -> Self {
        Self {
            storage: ra_salsa::Storage::default(),
            stats: QueryStatsCollector::default(),
            persistence: Persistence::new(project_root, persistence),
            file_paths: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn set_file_path(&mut self, file: FileId, path: impl Into<String>) {
        self.file_paths
            .write()
            .insert(file, Arc::new(path.into()));
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
        })
    }
}

/// Thread-safe handle around [`RootDatabase`].
///
/// - Writes are serialized through an internal `RwLock`.
/// - Reads are expected to happen through snapshots (`Database::snapshot`),
///   which can then be freely sent to worker threads.
#[derive(Clone, Default)]
pub struct Database {
    inner: Arc<RwLock<RootDatabase>>,
}

impl Database {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn new_with_persistence(
        project_root: impl AsRef<Path>,
        persistence: PersistenceConfig,
    ) -> Self {
        Self {
            inner: Arc::new(RwLock::new(RootDatabase::new_with_persistence(
                project_root,
                persistence,
            ))),
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.inner.read().snapshot()
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
        self.inner.read().query_stats()
    }

    pub fn clear_query_stats(&self) {
        self.inner.read().clear_query_stats();
    }

    pub fn persistence_stats(&self) -> crate::PersistenceStats {
        self.inner.read().persistence_stats()
    }

    pub fn with_write<T>(&self, f: impl FnOnce(&mut RootDatabase) -> T) -> T {
        let mut db = self.inner.write();
        f(&mut db)
    }

    pub fn request_cancellation(&self) {
        self.inner.write().request_cancellation();
    }

    pub fn set_file_exists(&self, file: FileId, exists: bool) {
        self.inner.write().set_file_exists(file, exists);
    }

    pub fn set_file_content(&self, file: FileId, content: Arc<String>) {
        self.inner.write().set_file_content(file, content);
    }

    pub fn set_file_text(&self, file: FileId, text: impl Into<String>) {
        let text = Arc::new(text.into());
        let mut db = self.inner.write();
        db.set_file_exists(file, true);
        db.set_source_root(file, SourceRootId::from_raw(0));
        db.set_file_content(file, text);
    }

    pub fn set_file_path(&self, file: FileId, path: impl Into<String>) {
        self.inner.write().set_file_path(file, path);
    }

    pub fn set_project_config(&self, project: ProjectId, config: Arc<ProjectConfig>) {
        self.inner.write().set_project_config(project, config);
    }

    pub fn set_source_root(&self, file: FileId, root: SourceRootId) {
        self.inner.write().set_source_root(file, root);
    }
}

/// Convenience trait alias that composes Nova's query groups.
pub trait NovaDatabase: NovaInputs + NovaSyntax + NovaSemantic + NovaIde + NovaHir {}

impl<T> NovaDatabase for T where T: NovaInputs + NovaSyntax + NovaSemantic + NovaIde + NovaHir {}

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
    use nova_cache::CacheConfig;
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
        assert_eq!(
            executions(&db, "hir_symbol_names"),
            1,
            "dependent query should be reused due to early-cutoff"
        );
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
            let _guard = cancellation::test_support::install_entered_long_running_region_sender(
                entered_tx,
            );
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

        let stats = db2.persistence_stats();
        assert!(
            stats.ast_load_misses > 0 && stats.ast_store_success > 0,
            "expected corruption to be treated as miss + recompute: {stats:?}"
        );
    }
}
