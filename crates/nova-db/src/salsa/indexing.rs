use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use nova_cache::Fingerprint;
use nova_hir::token_item_tree::TokenSymbolSummary;
use nova_index::{ProjectIndexes, SymbolLocation};

use crate::{FileId, ProjectId};

use crate::persistence::HasPersistence;

use super::cancellation as cancel;
use super::semantic::NovaSemantic;
use super::stats::HasQueryStats;

#[ra_salsa::query_group(NovaIndexingStorage)]
pub trait NovaIndexing: NovaSemantic + HasQueryStats + HasPersistence {
    /// Stable SHA-256 fingerprint of a file's current contents.
    fn file_fingerprint(&self, file: FileId) -> Arc<Fingerprint>;

    /// Map of `file_rel_path` → `file_fingerprint` for all existing project files.
    fn project_file_fingerprints(&self, project: ProjectId) -> Arc<BTreeMap<String, Fingerprint>>;

    /// Range-insensitive per-file symbol summary used for early-cutoff indexing.
    fn file_symbol_summary(&self, file: FileId) -> Arc<TokenSymbolSummary>;

    /// Index contributions for a single file.
    fn file_index_delta(&self, file: FileId) -> Arc<ProjectIndexes>;

    /// Project-wide indexes built by merging per-file deltas, warm-starting from disk when possible.
    fn project_indexes(&self, project: ProjectId) -> Arc<ProjectIndexes>;

    /// Convenience downstream query used by tests to validate early-cutoff behavior.
    fn project_symbol_count(&self, project: ProjectId) -> usize;
}

fn file_fingerprint(db: &dyn NovaIndexing, file: FileId) -> Arc<Fingerprint> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "file_fingerprint", ?file).entered();

    cancel::check_cancelled(db);

    let fp = if db.file_exists(file) {
        let text = db.file_content(file);
        Fingerprint::from_bytes(text.as_bytes())
    } else {
        Fingerprint::from_bytes([])
    };

    let result = Arc::new(fp);
    db.record_query_stat("file_fingerprint", start.elapsed());
    result
}

fn project_file_fingerprints(
    db: &dyn NovaIndexing,
    project: ProjectId,
) -> Arc<BTreeMap<String, Fingerprint>> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span =
        tracing::debug_span!("query", name = "project_file_fingerprints", ?project).entered();

    cancel::check_cancelled(db);

    let mut map = BTreeMap::new();
    for &file in db.project_files(project).iter() {
        if !db.file_exists(file) {
            continue;
        }
        let path = db.file_rel_path(file);
        let fp = db.file_fingerprint(file);
        map.insert(path.as_ref().clone(), fp.as_ref().clone());
    }

    let result = Arc::new(map);
    db.record_query_stat("project_file_fingerprints", start.elapsed());
    result
}

fn file_symbol_summary(db: &dyn NovaIndexing, file: FileId) -> Arc<TokenSymbolSummary> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "file_symbol_summary", ?file).entered();

    cancel::check_cancelled(db);

    let summary = db.symbol_summary(file);
    db.record_query_stat("file_symbol_summary", start.elapsed());
    summary
}

fn file_index_delta(db: &dyn NovaIndexing, file: FileId) -> Arc<ProjectIndexes> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "file_index_delta", ?file).entered();

    cancel::check_cancelled(db);

    let mut out = ProjectIndexes::default();
    if db.file_exists(file) {
        let rel_path = db.file_rel_path(file);
        let summary = db.file_symbol_summary(file);
        for name in &summary.names {
            out.symbols.insert(
                name.clone(),
                SymbolLocation {
                    file: rel_path.as_ref().clone(),
                    line: 0,
                    column: 0,
                },
            );
        }
    }

    let result = Arc::new(out);
    db.record_query_stat("file_index_delta", start.elapsed());
    result
}

fn project_indexes(db: &dyn NovaIndexing, project: ProjectId) -> Arc<ProjectIndexes> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "project_indexes", ?project).entered();

    cancel::check_cancelled(db);

    let file_fingerprints = db.project_file_fingerprints(project);

    let persistence = db.persistence();
    let cache_dir = persistence.cache_dir();
    let loaded = if persistence.mode().allows_read() {
        match cache_dir {
            Some(cache_dir) => {
                match nova_index::load_indexes_with_fingerprints(
                    cache_dir,
                    file_fingerprints.as_ref(),
                ) {
                    Ok(Some(loaded)) => {
                        db.record_disk_cache_hit("project_indexes");
                        Some(loaded)
                    }
                    Ok(None) => {
                        db.record_disk_cache_miss("project_indexes");
                        None
                    }
                    Err(_) => {
                        db.record_disk_cache_miss("project_indexes");
                        None
                    }
                }
            }
            None => None,
        }
    } else {
        None
    };

    let mut indexes = loaded
        .as_ref()
        .map(|loaded| loaded.indexes.clone())
        .unwrap_or_default();

    if let Some(loaded) = loaded {
        // Warm-start: only (re)index files that are new/changed since the persisted metadata.
        let mut path_to_file = BTreeMap::<String, FileId>::new();
        for &file in db.project_files(project).iter() {
            if !db.file_exists(file) {
                continue;
            }
            let path = db.file_rel_path(file);
            path_to_file.insert(path.as_ref().clone(), file);
        }

        for path in loaded.invalidated_files {
            let Some(&file) = path_to_file.get(&path) else {
                continue;
            };
            let delta = db.file_index_delta(file);
            merge_project_indexes(&mut indexes, delta.as_ref());
        }
    } else {
        // Cold start: build the project indexes by merging all file deltas.
        for &file in db.project_files(project).iter() {
            if !db.file_exists(file) {
                continue;
            }
            let delta = db.file_index_delta(file);
            merge_project_indexes(&mut indexes, delta.as_ref());
        }
    }

    // Persisted indexes carry an internal "generation" used to validate on-disk
    // archives. It is not semantically relevant for Salsa queries, so normalize
    // it to keep warm-start and cold-start results comparable.
    indexes.set_generation(0);

    let result = Arc::new(indexes);
    db.record_query_stat("project_indexes", start.elapsed());
    result
}

fn project_symbol_count(db: &dyn NovaIndexing, project: ProjectId) -> usize {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "project_symbol_count", ?project).entered();

    cancel::check_cancelled(db);

    let count = db.project_indexes(project).symbols.symbols.len();
    db.record_query_stat("project_symbol_count", start.elapsed());
    count
}

fn merge_project_indexes(into: &mut ProjectIndexes, delta: &ProjectIndexes) {
    for (name, locations) in &delta.symbols.symbols {
        into.symbols
            .symbols
            .entry(name.clone())
            .or_default()
            .extend(locations.iter().cloned());
    }

    for (name, locations) in &delta.references.references {
        into.references
            .references
            .entry(name.clone())
            .or_default()
            .extend(locations.iter().cloned());
    }

    for (name, locations) in &delta.annotations.annotations {
        into.annotations
            .annotations
            .entry(name.clone())
            .or_default()
            .extend(locations.iter().cloned());
    }

    // `InheritanceIndex` retains file associations in a private edge list; the
    // incremental indexing layer currently only populates symbol/reference/
    // annotation indexes, so we don't merge inheritance data here yet.
}
