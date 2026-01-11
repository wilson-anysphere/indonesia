use std::sync::Arc;
use std::time::Instant;

use nova_hir::{item_tree as build_item_tree, ItemTree, SymbolSummary};
use nova_syntax::{GreenNode, JavaParseResult, ParseResult};

use crate::FileId;

use super::inputs::NovaInputs;
use super::stats::HasQueryStats;

/// The parsed syntax tree type exposed by the database.
pub type SyntaxTree = GreenNode;

#[cfg(test)]
pub(crate) static INTERRUPTIBLE_WORK_STARTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

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
