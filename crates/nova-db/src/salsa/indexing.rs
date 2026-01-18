use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Instant;

use nova_cache::{Fingerprint, ProjectSnapshot};
use nova_index::{
    build_file_indexes, extract_java_file_index_extras, shard_id_for_path, JavaFileIndexExtras,
    ProjectIndexes, DEFAULT_SHARD_COUNT,
};

use crate::{FileId, ProjectId};

use crate::persistence::HasPersistence;

use super::cancellation as cancel;
use super::hir::NovaHir;
use super::stats::HasQueryStats;
use super::{HasSalsaMemoStats, TrackedSalsaMemo, TrackedSalsaProjectMemo};

#[ra_salsa::query_group(NovaIndexingStorage)]
pub trait NovaIndexing: NovaHir + HasQueryStats + HasPersistence + HasSalsaMemoStats {
    /// Stable SHA-256 fingerprint of a file's current contents.
    fn file_fingerprint(&self, file: FileId) -> Arc<Fingerprint>;

    /// Map of `file_rel_path` → `file_fingerprint` for all existing project files.
    fn project_file_fingerprints(&self, project: ProjectId) -> Arc<BTreeMap<String, Fingerprint>>;

    /// Range-insensitive per-file index extras (annotations, inheritance) used for early-cutoff
    /// indexing.
    fn file_index_extras(&self, file: FileId) -> Arc<JavaFileIndexExtras>;

    /// Index contributions for a single file.
    fn file_index_delta(&self, file: FileId) -> Arc<ProjectIndexes>;

    /// Project-wide sharded indexes built by merging per-file deltas, warm-starting from disk when
    /// possible.
    ///
    /// The returned vector is always ordered by shard id and always has length
    /// [`DEFAULT_SHARD_COUNT`].
    fn project_index_shards(&self, project: ProjectId) -> Arc<Vec<ProjectIndexes>>;

    /// Convenience query that returns the merged project indexes across all shards.
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
    for (idx, &file) in db.project_files(project).iter().enumerate() {
        cancel::checkpoint_cancelled(db, idx as u32);
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

fn file_index_extras(db: &dyn NovaIndexing, file: FileId) -> Arc<JavaFileIndexExtras> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "file_index_extras", ?file).entered();

    cancel::check_cancelled(db);

    let extras = if db.file_exists(file) {
        let parse = db.parse_java(file);
        extract_java_file_index_extras(parse.as_ref())
    } else {
        JavaFileIndexExtras::default()
    };

    let result = Arc::new(extras);
    db.record_query_stat("file_index_extras", start.elapsed());
    result
}

fn file_index_delta(db: &dyn NovaIndexing, file: FileId) -> Arc<ProjectIndexes> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "file_index_delta", ?file).entered();

    cancel::check_cancelled(db);

    let (out, approx_bytes) = if db.file_exists(file) {
        let rel_path = db.file_rel_path(file);
        let hir = db.hir_item_tree(file);
        let extras = db.file_index_extras(file);
        let out = build_file_indexes(rel_path.as_ref(), hir.as_ref(), extras.as_ref());
        let approx_bytes = out.estimated_bytes();
        (out, approx_bytes)
    } else {
        (ProjectIndexes::default(), 0)
    };

    let result = Arc::new(out);
    db.record_salsa_memo_bytes(file, TrackedSalsaMemo::FileIndexDelta, approx_bytes);
    db.record_query_stat("file_index_delta", start.elapsed());
    result
}

fn project_index_shards(db: &dyn NovaIndexing, project: ProjectId) -> Arc<Vec<ProjectIndexes>> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "project_index_shards", ?project).entered();

    cancel::check_cancelled(db);

    let shard_count = DEFAULT_SHARD_COUNT;

    let persistence = db.persistence();
    let cache_dir = persistence.cache_dir();

    let can_warm_start = persistence.mode().allows_read() && cache_dir.is_some();

    let mut path_to_file = BTreeMap::<String, FileId>::new();

    let fast_snapshot = if can_warm_start {
        let cache_dir = cache_dir.as_ref().expect("cache_dir checked above");
        static INDEXING_METADATA_FINGERPRINT_ERROR_LOGGED: OnceLock<()> = OnceLock::new();

        let mut fingerprints = BTreeMap::new();

        for (idx, &file) in db.project_files(project).iter().enumerate() {
            cancel::checkpoint_cancelled(db, idx as u32);
            if !db.file_exists(file) {
                continue;
            }

            let rel = db.file_rel_path(file);
            let path = rel.as_ref().clone();
            path_to_file.insert(path.clone(), file);

            let fp = if db.file_is_dirty(file) {
                db.file_fingerprint(file).as_ref().clone()
            } else {
                let full_path = cache_dir.project_root().join(&path);
                match Fingerprint::from_file_metadata(&full_path) {
                    Ok(fp) => fp,
                    Err(err) => {
                        match &err {
                            nova_cache::CacheError::Io(io_err)
                                if io_err.kind() == std::io::ErrorKind::NotFound => {}
                            _ => {
                                if INDEXING_METADATA_FINGERPRINT_ERROR_LOGGED.set(()).is_ok() {
                                    tracing::debug!(
                                        target = "nova.db",
                                        path = %full_path.display(),
                                        error = %err,
                                        "failed to fingerprint file by mtime/size for warm-start index; falling back to content fingerprint"
                                    );
                                }
                            }
                        }
                        db.file_fingerprint(file).as_ref().clone()
                    }
                }
            };
            fingerprints.insert(path, fp);
        }

        Some(ProjectSnapshot::from_parts(
            cache_dir.project_root().to_path_buf(),
            cache_dir.project_hash().clone(),
            fingerprints,
        ))
    } else {
        for (idx, &file) in db.project_files(project).iter().enumerate() {
            cancel::checkpoint_cancelled(db, idx as u32);
            if !db.file_exists(file) {
                continue;
            }

            let rel = db.file_rel_path(file);
            path_to_file.insert(rel.as_ref().clone(), file);
        }
        None
    };

    let mut shards = vec![ProjectIndexes::default(); shard_count as usize];
    let mut invalidated_files: Vec<String> = path_to_file.keys().cloned().collect();

    // Load persisted shards using the fast snapshot when persistence is enabled.
    let mut attempted_disk_cache = false;
    let mut used_disk_cache = false;

    if can_warm_start {
        attempted_disk_cache = true;
        let cache_dir = cache_dir.as_ref().expect("cache_dir checked above");
        let fast_snapshot = fast_snapshot.as_ref().expect("snapshot built above");

        let loaded = match nova_index::load_sharded_index_view_lazy_from_fast_snapshot(
            cache_dir,
            fast_snapshot,
            shard_count,
        ) {
            Ok(value) => value,
            Err(_) => None,
        };

        if let Some(loaded) = loaded {
            invalidated_files = loaded.invalidated_files;

            // Ensure dirty (in-memory modified) files are reindexed even when their on-disk
            // metadata fingerprints are unchanged.
            let mut invalidated: std::collections::BTreeSet<String> =
                invalidated_files.into_iter().collect();
            for (idx, &file) in db.project_files(project).iter().enumerate() {
                cancel::checkpoint_cancelled(db, idx as u32);
                if db.file_is_dirty(file) {
                    let rel_path = db.file_rel_path(file);
                    invalidated.insert(rel_path.as_ref().clone());
                }
            }
            invalidated_files = invalidated.into_iter().collect();

            // If every existing file is invalidated, we don't need to load any persisted shards:
            // we'll rebuild everything from scratch anyway.
            let invalidated_set: std::collections::HashSet<&str> =
                invalidated_files.iter().map(|path| path.as_str()).collect();
            let indexing_all_files = path_to_file
                .keys()
                .all(|path| invalidated_set.contains(path.as_str()));

            if !indexing_all_files {
                let mut loaded_shards = Vec::with_capacity(shard_count as usize);
                let mut corrupt_shards = std::collections::BTreeSet::new();

                // Only shards that contain at least one unchanged file need to be loaded from disk.
                // Shards where all files are invalidated can be rebuilt from scratch.
                let mut shard_has_unchanged = vec![false; shard_count as usize];
                for path in path_to_file.keys() {
                    if invalidated_set.contains(path.as_str()) {
                        continue;
                    }
                    let shard_id = shard_id_for_path(path, shard_count) as usize;
                    shard_has_unchanged[shard_id] = true;
                }

                for shard_id in 0..shard_count {
                    cancel::checkpoint_cancelled(db, shard_id);

                    let indexes = if shard_has_unchanged[shard_id as usize] {
                        match loaded.view.shard(shard_id) {
                            Some(archives) => {
                                let Ok(symbols) = archives.symbols.to_owned() else {
                                    corrupt_shards.insert(shard_id);
                                    loaded_shards.push(ProjectIndexes::default());
                                    continue;
                                };
                                let Ok(references) = archives.references.to_owned() else {
                                    corrupt_shards.insert(shard_id);
                                    loaded_shards.push(ProjectIndexes::default());
                                    continue;
                                };
                                let Ok(inheritance) = archives.inheritance.to_owned() else {
                                    corrupt_shards.insert(shard_id);
                                    loaded_shards.push(ProjectIndexes::default());
                                    continue;
                                };
                                let Ok(annotations) = archives.annotations.to_owned() else {
                                    corrupt_shards.insert(shard_id);
                                    loaded_shards.push(ProjectIndexes::default());
                                    continue;
                                };

                                ProjectIndexes {
                                    symbols,
                                    references,
                                    inheritance,
                                    annotations,
                                }
                            }
                            None => {
                                corrupt_shards.insert(shard_id);
                                ProjectIndexes::default()
                            }
                        }
                    } else {
                        ProjectIndexes::default()
                    };
                    loaded_shards.push(indexes);
                }

                if loaded_shards.len() == shard_count as usize {
                    used_disk_cache = true;
                    shards = loaded_shards;

                    // If we had to fall back to a default shard (due to corruption while
                    // materializing an archive), force all files that map to that shard to be
                    // reindexed.
                    if !corrupt_shards.is_empty() {
                        let mut invalidated: std::collections::BTreeSet<String> =
                            invalidated_files.into_iter().collect();
                        for path in path_to_file.keys() {
                            let shard_id = shard_id_for_path(path, shard_count);
                            if corrupt_shards.contains(&shard_id) {
                                invalidated.insert(path.clone());
                            }
                        }
                        invalidated_files = invalidated.into_iter().collect();
                    }
                }
            }
        }
    }

    if attempted_disk_cache {
        if used_disk_cache {
            db.record_disk_cache_hit("project_indexes");
        } else {
            db.record_disk_cache_miss("project_indexes");
        }
    }

    // Reindex only invalidated files and update their target shards.
    for (idx, path) in invalidated_files.into_iter().enumerate() {
        cancel::checkpoint_cancelled(db, idx as u32);
        let shard = shard_id_for_path(&path, shard_count) as usize;
        let indexes = shards
            .get_mut(shard)
            .expect("shard_id_for_path always returns < shard_count");
        indexes.invalidate_file(&path);

        if let Some(&file) = path_to_file.get(&path) {
            let delta = db.file_index_delta(file);
            indexes.merge_from((*delta).clone());
        }
    }

    // Persisted indexes carry an internal "generation" used for validation and
    // cache compaction. It is not semantically relevant for Salsa queries, so
    // normalize it to keep warm-start and cold-start results comparable.
    for shard in &mut shards {
        shard.set_generation(0);
    }

    let approx_bytes = shards
        .iter()
        .map(ProjectIndexes::estimated_bytes)
        .fold(0u64, u64::saturating_add);

    let result = Arc::new(shards);
    db.record_salsa_project_memo_bytes(
        project,
        TrackedSalsaProjectMemo::ProjectIndexShards,
        approx_bytes,
    );
    db.record_query_stat("project_index_shards", start.elapsed());
    result
}

fn project_indexes(db: &dyn NovaIndexing, project: ProjectId) -> Arc<ProjectIndexes> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "project_indexes", ?project).entered();

    cancel::check_cancelled(db);

    let shards = db.project_index_shards(project);
    let mut indexes = ProjectIndexes::default();
    for shard in shards.iter() {
        indexes.merge_from(shard.clone());
    }
    // Ensure equality between warm-start and cold-start outputs by normalizing any persisted
    // generation marker.
    indexes.set_generation(0);

    let approx_bytes = indexes.estimated_bytes();
    let result = Arc::new(indexes);
    db.record_salsa_project_memo_bytes(
        project,
        TrackedSalsaProjectMemo::ProjectIndexes,
        approx_bytes,
    );
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
