//! Salsa-powered incremental query database for Nova.
//!
//! This is the foundation for Nova's incremental computation engine:
//! - input queries for file content/existence and project configuration
//! - derived queries for parsing and per-file structural summaries
//! - snapshot-based concurrency (via `ra_salsa::ParallelDatabase`)
//! - lightweight instrumentation (query timings + optional `tracing`)
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

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use parking_lot::RwLock;

use nova_hir::{item_tree as build_item_tree, ItemTree, SymbolSummary};
use nova_project::ProjectConfig;
use nova_syntax::{GreenNode, JavaParseResult, ParseResult};

use crate::{FileId, ProjectId};

/// The parsed syntax tree type exposed by the database.
pub type SyntaxTree = GreenNode;

#[cfg(test)]
static INTERRUPTIBLE_WORK_STARTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Database functionality needed by query implementations to record timing stats.
pub trait HasQueryStats {
    fn record_query_stat(&self, query_name: &'static str, duration: Duration);
}

/// Runs `f` and catches any Salsa cancellation.
///
/// This is a convenience wrapper around `ra_salsa::Cancelled::catch`.
pub fn catch_cancelled<F, T>(f: F) -> Result<T, ra_salsa::Cancelled>
where
    F: FnOnce() -> T + std::panic::UnwindSafe,
{
    ra_salsa::Cancelled::catch(f)
}

#[ra_salsa::query_group(NovaInputsStorage)]
pub trait NovaInputs: ra_salsa::Database {
    /// File content as last provided by the host (e.g. LSP text document sync).
    #[ra_salsa::input]
    fn file_content(&self, file: FileId) -> Arc<String>;

    /// Whether a file exists on disk (or in the VFS).
    #[ra_salsa::input]
    fn file_exists(&self, file: FileId) -> bool;

    /// Per-project configuration input (classpath, source roots, language level, ...).
    #[ra_salsa::input]
    fn project_config(&self, project: ProjectId) -> Arc<ProjectConfig>;
}

#[ra_salsa::query_group(NovaSyntaxStorage)]
pub trait NovaSyntax: NovaInputs + HasQueryStats {
    /// Parse a file into a syntax tree (memoized and dependency-tracked).
    fn parse(&self, file: FileId) -> Arc<ParseResult>;

    /// Parse a file using the full-fidelity Rowan-based Java grammar.
    fn parse_java(&self, file: FileId) -> Arc<JavaParseResult>;

    /// Convenience query that exposes the syntax tree.
    fn syntax_tree(&self, file: FileId) -> Arc<SyntaxTree>;

    /// Structural, trivia-insensitive per-file summary used by name resolution.
    ///
    /// This is the canonical "early-cutoff" demo: whitespace edits re-run `parse`
    /// but generally keep `item_tree` identical, which avoids recomputing its
    /// dependents.
    fn item_tree(&self, file: FileId) -> Arc<ItemTree>;

    /// Further derived query (depends on `item_tree`) used by tests to verify
    /// early-cutoff.
    fn symbol_summary(&self, file: FileId) -> Arc<SymbolSummary>;

    /// Dummy downstream query used by tests to validate early-cutoff behavior.
    fn symbol_count(&self, file: FileId) -> usize;

    /// Debug query used to validate request cancellation behavior.
    ///
    /// Real queries (type-checking, indexing, etc.) should periodically call
    /// `db.unwind_if_cancelled()` while doing expensive work; this query exists
    /// as a lightweight fixture for that pattern.
    fn interruptible_work(&self, file: FileId, steps: u32) -> u64;
}

fn parse(db: &dyn NovaSyntax, file: FileId) -> Arc<ParseResult> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "parse", ?file).entered();

    db.unwind_if_cancelled();

    let text = if db.file_exists(file) {
        db.file_content(file)
    } else {
        Arc::new(String::new())
    };

    let parsed = nova_syntax::parse(text.as_str());
    let result = Arc::new(parsed);
    db.record_query_stat("parse", start.elapsed());
    result
}

fn parse_java(db: &dyn NovaSyntax, file: FileId) -> Arc<JavaParseResult> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "parse_java", ?file).entered();

    db.unwind_if_cancelled();

    let text = if db.file_exists(file) {
        db.file_content(file)
    } else {
        Arc::new(String::new())
    };

    let parsed = nova_syntax::parse_java(text.as_str());
    let result = Arc::new(parsed);
    db.record_query_stat("parse_java", start.elapsed());
    result
}

fn syntax_tree(db: &dyn NovaSyntax, file: FileId) -> Arc<SyntaxTree> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "syntax_tree", ?file).entered();

    db.unwind_if_cancelled();

    let root = db.parse(file).root.clone();
    let result = Arc::new(root);
    db.record_query_stat("syntax_tree", start.elapsed());
    result
}

fn item_tree(db: &dyn NovaSyntax, file: FileId) -> Arc<ItemTree> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "item_tree", ?file).entered();

    db.unwind_if_cancelled();

    let parse = db.parse(file);
    let text = if db.file_exists(file) {
        db.file_content(file)
    } else {
        Arc::new(String::new())
    };
    let it = build_item_tree(&parse, text.as_str());
    let result = Arc::new(it);
    db.record_query_stat("item_tree", start.elapsed());
    result
}

fn symbol_summary(db: &dyn NovaSyntax, file: FileId) -> Arc<SymbolSummary> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "symbol_summary", ?file).entered();

    db.unwind_if_cancelled();

    let it = db.item_tree(file);
    let summary = SymbolSummary::from_item_tree(&it);
    let result = Arc::new(summary);
    db.record_query_stat("symbol_summary", start.elapsed());
    result
}

fn symbol_count(db: &dyn NovaSyntax, file: FileId) -> usize {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "symbol_count", ?file).entered();

    db.unwind_if_cancelled();

    let count = db.symbol_summary(file).names.len();
    db.record_query_stat("symbol_count", start.elapsed());
    count
}

fn interruptible_work(db: &dyn NovaSyntax, file: FileId, steps: u32) -> u64 {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "interruptible_work", ?file, steps).entered();

    #[cfg(test)]
    INTERRUPTIBLE_WORK_STARTED.store(true, std::sync::atomic::Ordering::SeqCst);

    let mut acc: u64 = 0;
    for i in 0..steps {
        if i % 256 == 0 {
            db.unwind_if_cancelled();
        }
        acc = acc.wrapping_add(i as u64 ^ file.to_raw() as u64);
        std::hint::black_box(acc);
    }

    db.record_query_stat("interruptible_work", start.elapsed());
    acc
}

/// Read-only snapshot type for concurrent query execution.
pub type Snapshot = ra_salsa::Snapshot<QueryDatabase>;

/// Lightweight query timing/execution stats.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueryStats {
    pub by_query: BTreeMap<String, QueryStat>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueryStat {
    pub executions: u64,
    pub total_time: Duration,
    pub max_time: Duration,
}

#[derive(Clone, Default)]
struct QueryStatsCollector {
    inner: Arc<Mutex<BTreeMap<String, QueryStat>>>,
}

impl QueryStatsCollector {
    fn record(&self, key: String, duration: Duration) {
        let mut guard = self.inner.lock().expect("query stats mutex poisoned");
        let entry = guard.entry(key).or_default();
        entry.executions = entry.executions.saturating_add(1);
        entry.total_time = entry.total_time.saturating_add(duration);
        entry.max_time = entry.max_time.max(duration);
    }

    fn snapshot(&self) -> QueryStats {
        let guard = self.inner.lock().expect("query stats mutex poisoned");
        QueryStats {
            by_query: guard.clone(),
        }
    }

    fn clear(&self) {
        self.inner
            .lock()
            .expect("query stats mutex poisoned")
            .clear();
    }
}

/// The concrete Salsa database for Nova.
#[ra_salsa::database(NovaInputsStorage, NovaSyntaxStorage)]
pub struct QueryDatabase {
    storage: ra_salsa::Storage<QueryDatabase>,
    stats: QueryStatsCollector,
}

impl Default for QueryDatabase {
    fn default() -> Self {
        Self {
            storage: ra_salsa::Storage::default(),
            stats: QueryStatsCollector::default(),
        }
    }
}

impl QueryDatabase {
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

impl HasQueryStats for QueryDatabase {
    fn record_query_stat(&self, query_name: &'static str, duration: Duration) {
        self.stats.record(query_name.to_string(), duration);
    }
}

impl ra_salsa::Database for QueryDatabase {
    fn salsa_event(&self, event: ra_salsa::Event) {
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

impl ra_salsa::ParallelDatabase for QueryDatabase {
    fn snapshot(&self) -> ra_salsa::Snapshot<QueryDatabase> {
        ra_salsa::Snapshot::new(QueryDatabase {
            storage: self.storage.snapshot(),
            stats: self.stats.clone(),
        })
    }
}

/// Thread-safe handle around [`QueryDatabase`].
///
/// - Writes are serialized through an internal `RwLock`.
/// - Reads are expected to happen through snapshots (`Database::snapshot`),
///   which can then be freely sent to worker threads.
#[derive(Clone, Default)]
pub struct Database {
    inner: Arc<RwLock<QueryDatabase>>,
}

impl Database {
    pub fn new() -> Self {
        Self::default()
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
        f: impl FnOnce(&Snapshot) -> T + std::panic::UnwindSafe,
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

    pub fn with_write<T>(&self, f: impl FnOnce(&mut QueryDatabase) -> T) -> T {
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
        db.set_file_content(file, text);
    }

    pub fn set_project_config(&self, project: ProjectId, config: Arc<ProjectConfig>) {
        self.inner.write().set_project_config(project, config);
    }
}

/// Convenience trait alias that composes Nova's query groups.
pub trait NovaDatabase: NovaInputs + NovaSyntax {}

impl<T> NovaDatabase for T where T: NovaInputs + NovaSyntax {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    fn executions(db: &QueryDatabase, query_name: &str) -> u64 {
        db.query_stats()
            .by_query
            .get(query_name)
            .map(|s| s.executions)
            .unwrap_or(0)
    }

    #[test]
    fn edit_invalidates_parse() {
        let mut db = QueryDatabase::default();
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
        let mut db = QueryDatabase::default();
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
    fn snapshots_are_consistent_across_concurrent_reads() {
        let mut db = QueryDatabase::default();
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
        INTERRUPTIBLE_WORK_STARTED.store(false, Ordering::SeqCst);

        let mut db = QueryDatabase::default();
        let file = FileId::from_raw(1);
        db.set_file_exists(file, true);
        db.set_file_content(file, Arc::new("class Foo {}".to_string()));

        let snap = db.snapshot();
        let handle = std::thread::spawn(move || {
            ra_salsa::Cancelled::catch(|| snap.interruptible_work(file, 5_000_000))
        });

        while !INTERRUPTIBLE_WORK_STARTED.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }

        // This will block until `snap` is dropped; cancellation ensures that the
        // worker thread unwinds and releases its snapshot.
        db.request_cancellation();

        let result = handle.join().expect("worker thread panicked");
        assert!(
            result.is_err(),
            "expected salsa query to unwind with Cancelled after request_cancellation"
        );
    }
}
