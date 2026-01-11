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
mod inputs;
mod stats;
mod syntax;

pub use inputs::NovaInputs;
pub use stats::{HasQueryStats, QueryStat, QueryStats};
pub use syntax::{NovaSyntax, SyntaxTree};

use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;

use nova_project::ProjectConfig;

use crate::{FileId, ProjectId, SourceRootId};

use self::stats::QueryStatsCollector;

/// Runs `f` and catches any Salsa cancellation.
///
/// This is a convenience wrapper around `ra_salsa::Cancelled::catch`.
pub fn catch_cancelled<F, T>(f: F) -> Result<T, ra_salsa::Cancelled>
where
    F: FnOnce() -> T + std::panic::UnwindSafe,
{
    ra_salsa::Cancelled::catch(f)
}

/// Read-only snapshot type for concurrent query execution.
pub type Snapshot = ra_salsa::Snapshot<RootDatabase>;

/// The concrete Salsa database for Nova (the ADR 0001 "RootDatabase").
#[ra_salsa::database(inputs::NovaInputsStorage, syntax::NovaSyntaxStorage)]
pub struct RootDatabase {
    storage: ra_salsa::Storage<RootDatabase>,
    stats: QueryStatsCollector,
}

impl Default for RootDatabase {
    fn default() -> Self {
        Self {
            storage: ra_salsa::Storage::default(),
            stats: QueryStatsCollector::default(),
        }
    }
}

impl RootDatabase {
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
        self.stats.record(query_name.to_string(), duration);
    }
}

impl ra_salsa::Database for RootDatabase {
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

impl ra_salsa::ParallelDatabase for RootDatabase {
    fn snapshot(&self) -> ra_salsa::Snapshot<RootDatabase> {
        ra_salsa::Snapshot::new(RootDatabase {
            storage: self.storage.snapshot(),
            stats: self.stats.clone(),
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

    pub fn set_project_config(&self, project: ProjectId, config: Arc<ProjectConfig>) {
        self.inner.write().set_project_config(project, config);
    }

    pub fn set_source_root(&self, file: FileId, root: SourceRootId) {
        self.inner.write().set_source_root(file, root);
    }
}

/// Convenience trait alias that composes Nova's query groups.
pub trait NovaDatabase: NovaInputs + NovaSyntax {}

impl<T> NovaDatabase for T where T: NovaInputs + NovaSyntax {}

#[cfg(test)]
fn assert_query_is_cancelled<T, F>(mut db: RootDatabase, run_query: F)
where
    T: Send + 'static,
    F: FnOnce(&Snapshot) -> T + Send + std::panic::UnwindSafe + 'static,
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

    fn executions(db: &RootDatabase, query_name: &str) -> u64 {
        db.query_stats()
            .by_query
            .get(query_name)
            .map(|s| s.executions)
            .unwrap_or(0)
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

        assert_query_is_cancelled(db, move |snap| snap.synthetic_semantic_work(file, 5_000_000));
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
}

