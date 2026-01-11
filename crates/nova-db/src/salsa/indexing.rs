use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
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

#[ra_salsa::query_group(NovaIndexingStorage)]
pub trait NovaIndexing: NovaHir + HasQueryStats + HasPersistence {
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
    fn project_indexes(&self, project: ProjectId) -> Arc<Vec<ProjectIndexes>>;

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

    let out = if db.file_exists(file) {
        let rel_path = db.file_rel_path(file);
        let hir = db.hir_item_tree(file);
        let extras = db.file_index_extras(file);
        build_file_indexes(rel_path.as_ref(), hir.as_ref(), extras.as_ref())
    } else {
        ProjectIndexes::default()
    };

    let result = Arc::new(out);
    db.record_query_stat("file_index_delta", start.elapsed());
    result
}

fn project_indexes(db: &dyn NovaIndexing, project: ProjectId) -> Arc<Vec<ProjectIndexes>> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "project_indexes", ?project).entered();

    cancel::check_cancelled(db);

    let file_fingerprints = db.project_file_fingerprints(project);
    let shard_count = DEFAULT_SHARD_COUNT;

    let persistence = db.persistence();
    let cache_dir = persistence.cache_dir();

    let snapshot = cache_dir.map(|cache_dir| {
        ProjectSnapshot::from_parts(
            cache_dir.project_root().to_path_buf(),
            cache_dir.project_hash().clone(),
            file_fingerprints.as_ref().clone(),
        )
    });

    let loaded = if persistence.mode().allows_read() {
        match (cache_dir, snapshot.as_ref()) {
            (Some(cache_dir), Some(snapshot)) => match nova_index::load_sharded_index_archives(
                cache_dir,
                snapshot,
                shard_count,
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
            },
            _ => None,
        }
    } else {
        None
    };

    let mut shards = vec![ProjectIndexes::default(); shard_count as usize];
    let mut invalidated_files: Vec<String> = file_fingerprints.keys().cloned().collect();

    if let Some(loaded) = loaded {
        let mut loaded_shards = Vec::with_capacity(shard_count as usize);
        let mut ok = true;
        for shard in loaded.shards {
            let indexes = match shard {
                Some(archives) => {
                    let symbols = match archives.symbols.to_owned() {
                        Ok(value) => value,
                        Err(_) => {
                            ok = false;
                            break;
                        }
                    };
                    let references = match archives.references.to_owned() {
                        Ok(value) => value,
                        Err(_) => {
                            ok = false;
                            break;
                        }
                    };
                    let inheritance = match archives.inheritance.to_owned() {
                        Ok(value) => value,
                        Err(_) => {
                            ok = false;
                            break;
                        }
                    };
                    let annotations = match archives.annotations.to_owned() {
                        Ok(value) => value,
                        Err(_) => {
                            ok = false;
                            break;
                        }
                    };
                    ProjectIndexes {
                        symbols,
                        references,
                        inheritance,
                        annotations,
                    }
                }
                None => ProjectIndexes::default(),
            };
            loaded_shards.push(indexes);
        }

        if ok && loaded_shards.len() == shard_count as usize {
            shards = loaded_shards;
            invalidated_files = loaded.invalidated_files;
        }
    }

    // Remove stale results for invalidated (new/modified/deleted) files before re-indexing.
    for path in &invalidated_files {
        let shard = shard_id_for_path(path, shard_count) as usize;
        if let Some(indexes) = shards.get_mut(shard) {
            indexes.invalidate_file(path);
        }
    }

    // Warm-start: only (re)index files that are new/changed since the persisted metadata.
    let mut path_to_file = BTreeMap::<String, FileId>::new();
    for &file in db.project_files(project).iter() {
        if !db.file_exists(file) {
            continue;
        }
        let path = db.file_rel_path(file);
        path_to_file.insert(path.as_ref().clone(), file);
    }

    for path in invalidated_files {
        let Some(&file) = path_to_file.get(&path) else {
            continue;
        };
        let delta = db.file_index_delta(file);
        let shard = shard_id_for_path(&path, shard_count) as usize;
        shards[shard].merge_from((*delta).clone());
    }

    // Persisted indexes carry an internal "generation" used for validation and
    // cache compaction. It is not semantically relevant for Salsa queries, so
    // normalize it to keep warm-start and cold-start results comparable.
    for shard in &mut shards {
        shard.set_generation(0);
    }

    let result = Arc::new(shards);
    db.record_query_stat("project_indexes", start.elapsed());
    result
}

fn project_symbol_count(db: &dyn NovaIndexing, project: ProjectId) -> usize {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "project_symbol_count", ?project).entered();

    cancel::check_cancelled(db);

    let shards = db.project_indexes(project);
    let mut names = BTreeSet::new();
    for shard in shards.iter() {
        names.extend(shard.symbols.symbols.keys().cloned());
    }
    let count = names.len();
    db.record_query_stat("project_symbol_count", start.elapsed());
    count
}
