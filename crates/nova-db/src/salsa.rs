//! Salsa-powered incremental query database for Nova.
//!
//! This is the foundation for Nova's incremental computation engine:
//! - input queries for file content/existence and project configuration
//! - derived queries for parsing and per-file structural summaries
//! - snapshot-based concurrency (via `ra_salsa::ParallelDatabase`)
//! - lightweight instrumentation (query timings + optional `tracing`)

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nova_hir::{item_tree as build_item_tree, ItemTree, SymbolSummary};
use nova_project::ProjectConfig;
use nova_syntax::{GreenNode, ParseResult};

use crate::{FileId, ProjectId};

/// The parsed syntax tree type exposed by the database.
pub type SyntaxTree = GreenNode;

/// Database functionality needed by query implementations to record timing stats.
pub trait HasQueryStats {
    fn record_query_stat(&self, query_name: &'static str, duration: Duration);
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
    let text = db.file_content(file);
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

    /// Snapshot current query timing stats.
    #[inline]
    pub fn query_stats(&self) -> QueryStats {
        self.stats.snapshot()
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

/// Convenience trait alias that composes Nova's query groups.
pub trait NovaDatabase: NovaInputs + NovaSyntax {}

impl<T> NovaDatabase for T where T: NovaInputs + NovaSyntax {}

#[cfg(test)]
mod tests {
    use super::*;
    use ra_salsa::ParallelDatabase as _;

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

        let first = db.symbol_summary(file);
        assert_eq!(first.names, vec!["Foo".to_string()]);

        assert_eq!(executions(&db, "parse"), 1);
        assert_eq!(executions(&db, "item_tree"), 1);
        assert_eq!(executions(&db, "symbol_summary"), 1);

        // Whitespace-only change *after* the class name: parse changes (token ranges),
        // but the structural `item_tree` remains equal, so `symbol_summary` can be reused.
        db.set_file_content(file, Arc::new("class Foo {    }".to_string()));
        let second = db.symbol_summary(file);

        assert_eq!(second.names, first.names);
        assert_eq!(executions(&db, "parse"), 2);
        assert_eq!(executions(&db, "item_tree"), 2);
        assert_eq!(
            executions(&db, "symbol_summary"),
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
}
