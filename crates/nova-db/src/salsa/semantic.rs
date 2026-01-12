use std::sync::Arc;
use std::time::Instant;

use nova_cache::{FileAstArtifacts, Fingerprint};
use nova_hir::token_item_tree::{
    token_item_tree as build_item_tree, TokenItemTree, TokenSymbolSummary,
};

use crate::FileId;

use super::cancellation as cancel;
use super::stats::HasQueryStats;
use super::syntax::NovaSyntax;
use super::HasItemTreeStore;
use super::TrackedSalsaMemo;

#[ra_salsa::query_group(NovaSemanticStorage)]
pub trait NovaSemantic: NovaSyntax + HasQueryStats + HasItemTreeStore {
    /// Structural, trivia-insensitive per-file summary used by name resolution.
    ///
    /// This is the canonical "early-cutoff" demo: whitespace edits re-run `parse`
    /// but generally keep `item_tree` identical, which avoids recomputing its
    /// dependents.
    fn item_tree(&self, file: FileId) -> Arc<TokenItemTree>;

    /// Further derived query (depends on `item_tree`) used by tests to verify
    /// early-cutoff.
    fn symbol_summary(&self, file: FileId) -> Arc<TokenSymbolSummary>;

    /// Dummy downstream query used by tests to validate early-cutoff behavior.
    fn symbol_count(&self, file: FileId) -> usize;

    /// Synthetic long-running query that mimics future semantic analysis work.
    ///
    /// This intentionally does "real" Salsa work up front by depending on
    /// `symbol_summary`, then runs a tight loop with periodic cancellation
    /// checkpoints.
    fn synthetic_semantic_work(&self, file: FileId, steps: u32) -> u64;
}

fn item_tree(db: &dyn NovaSemantic, file: FileId) -> Arc<TokenItemTree> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "item_tree", ?file).entered();

    cancel::check_cancelled(db);

    let text = if db.file_exists(file) {
        db.file_content(file)
    } else {
        Arc::new(String::new())
    };
    let approx_bytes = text.len() as u64;

    let store = db.item_tree_store();
    if let Some(store) = store.as_ref() {
        if let Some(cached) = store.get_if_text_matches(file, &text) {
            db.record_salsa_memo_bytes(file, TrackedSalsaMemo::ItemTree, approx_bytes);
            db.record_query_stat("item_tree", start.elapsed());
            return cached;
        }
    }

    let file_path = db.file_path(file).filter(|p| !p.is_empty());
    let mode = db.persistence().mode();
    let fingerprint = if file_path.is_some() && mode.allows_read() {
        Some(Fingerprint::from_bytes(text.as_bytes()))
    } else {
        None
    };

    if let (Some(fingerprint), Some(file_path)) = (fingerprint.as_ref(), file_path.as_ref()) {
        match db
            .persistence()
            .load_ast_artifacts(file_path.as_str(), fingerprint)
        {
                Some(artifacts) => {
                    db.record_disk_cache_hit("item_tree");
                    let result = Arc::new(artifacts.item_tree);
                    if let Some(store) = store.as_ref() {
                        store.insert(file, text.clone(), result.clone());
                    }
                    db.record_salsa_memo_bytes(file, TrackedSalsaMemo::ItemTree, approx_bytes);
                    db.record_query_stat("item_tree", start.elapsed());
                    return result;
                }
                None => {
                    db.record_disk_cache_miss("item_tree");
                }
        }
    }

    let parse = db.parse(file);
    let it = build_item_tree(&parse, text.as_str());
    let result = Arc::new(it);
    if let Some(store) = store.as_ref() {
        store.insert(file, text.clone(), result.clone());
    }
    db.record_salsa_memo_bytes(file, TrackedSalsaMemo::ItemTree, approx_bytes);

    if let (Some(fingerprint), Some(file_path)) = (fingerprint.as_ref(), file_path.as_ref()) {
        let artifacts = FileAstArtifacts {
            parse: (*parse).clone(),
            item_tree: (*result).clone(),
            symbol_summary: None,
        };
        db.persistence()
            .store_ast_artifacts(file_path.as_str(), fingerprint, &artifacts);
    }
    db.record_query_stat("item_tree", start.elapsed());
    result
}

fn symbol_summary(db: &dyn NovaSemantic, file: FileId) -> Arc<TokenSymbolSummary> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "symbol_summary", ?file).entered();

    cancel::check_cancelled(db);

    let it = db.item_tree(file);
    let summary = TokenSymbolSummary::from_item_tree(&it);
    let result = Arc::new(summary);
    db.record_query_stat("symbol_summary", start.elapsed());
    result
}

fn symbol_count(db: &dyn NovaSemantic, file: FileId) -> usize {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "symbol_count", ?file).entered();

    cancel::check_cancelled(db);

    let count = db.symbol_summary(file).names.len();
    db.record_query_stat("symbol_count", start.elapsed());
    count
}

fn synthetic_semantic_work(db: &dyn NovaSemantic, file: FileId, steps: u32) -> u64 {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span =
        tracing::debug_span!("query", name = "synthetic_semantic_work", ?file, steps).entered();

    // Pull in some derived data to mimic the shape of future semantic queries.
    let summary = db.symbol_summary(file);

    let mut acc: u64 = 0;
    for i in 0..steps {
        cancel::checkpoint_cancelled(db, i);

        // Some deterministic "work" that depends on the summary.
        acc = acc.wrapping_add(i as u64 ^ file.to_raw() as u64);
        for name in summary.names.iter() {
            acc = acc.wrapping_add(name.len() as u64);
        }
        std::hint::black_box(acc);
    }

    db.record_query_stat("synthetic_semantic_work", start.elapsed());
    acc
}
