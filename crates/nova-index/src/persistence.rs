use crate::indexes::ProjectIndexes;
use nova_cache::{CacheDir, CacheMetadata, ProjectSnapshot};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const INDEX_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum IndexPersistenceError {
    #[error(transparent)]
    Cache(#[from] nova_cache::CacheError),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Bincode(#[from] bincode::Error),
}

#[derive(Clone, Debug)]
pub struct LoadedIndexes {
    pub indexes: ProjectIndexes,
    pub invalidated_files: Vec<String>,
}

pub fn save_indexes(
    cache_dir: &CacheDir,
    snapshot: &ProjectSnapshot,
    indexes: &ProjectIndexes,
) -> Result<(), IndexPersistenceError> {
    let indexes_dir = cache_dir.indexes_dir();
    std::fs::create_dir_all(&indexes_dir)?;

    write_index_file(indexes_dir.join("symbols.idx"), &indexes.symbols)?;
    write_index_file(indexes_dir.join("references.idx"), &indexes.references)?;
    write_index_file(indexes_dir.join("inheritance.idx"), &indexes.inheritance)?;
    write_index_file(indexes_dir.join("annotations.idx"), &indexes.annotations)?;

    let metadata_path = cache_dir.metadata_path();
    let mut metadata = match CacheMetadata::load(&metadata_path) {
        Ok(existing) if existing.is_compatible() && &existing.project_hash == snapshot.project_hash() => {
            existing
        }
        _ => CacheMetadata::new(snapshot),
    };
    metadata.update_from_snapshot(snapshot);
    metadata.save(metadata_path)?;
    Ok(())
}

pub fn load_indexes(
    cache_dir: &CacheDir,
    current_snapshot: &ProjectSnapshot,
) -> Result<Option<LoadedIndexes>, IndexPersistenceError> {
    let metadata_path = cache_dir.metadata_path();
    if !metadata_path.exists() {
        return Ok(None);
    }
    let metadata = match CacheMetadata::load(metadata_path) {
        Ok(metadata) => metadata,
        Err(_) => return Ok(None),
    };
    if !metadata.is_compatible() {
        return Ok(None);
    }
    if &metadata.project_hash != current_snapshot.project_hash() {
        return Ok(None);
    }

    let indexes_dir = cache_dir.indexes_dir();
    let Some(symbols) = read_index_file(indexes_dir.join("symbols.idx"))? else {
        return Ok(None);
    };
    let Some(references) = read_index_file(indexes_dir.join("references.idx"))? else {
        return Ok(None);
    };
    let Some(inheritance) = read_index_file(indexes_dir.join("inheritance.idx"))? else {
        return Ok(None);
    };
    let Some(annotations) = read_index_file(indexes_dir.join("annotations.idx"))? else {
        return Ok(None);
    };

    let mut indexes = ProjectIndexes {
        symbols,
        references,
        inheritance,
        annotations,
    };

    let invalidated = metadata.diff_files(current_snapshot);

    for file in &invalidated {
        indexes.invalidate_file(file);
    }

    Ok(Some(LoadedIndexes {
        indexes,
        invalidated_files: invalidated,
    }))
}

#[derive(Serialize, Deserialize)]
struct PersistedIndex<T> {
    schema_version: u32,
    nova_version: String,
    payload: T,
}

fn write_index_file<T: Serialize>(path: PathBuf, payload: &T) -> Result<(), IndexPersistenceError> {
    let persisted = PersistedIndex {
        schema_version: INDEX_SCHEMA_VERSION,
        nova_version: nova_core::NOVA_VERSION.to_string(),
        payload,
    };

    let bytes = bincode::serialize(&persisted)?;
    nova_cache::atomic_write(&path, &bytes)?;
    Ok(())
}

fn read_index_file<T: for<'de> Deserialize<'de>>(
    path: PathBuf,
) -> Result<Option<T>, IndexPersistenceError> {
    if !path.exists() {
        return Ok(None);
    }

    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(None),
    };
    let persisted: PersistedIndex<T> = match bincode::deserialize(&bytes) {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    if persisted.schema_version != INDEX_SCHEMA_VERSION {
        return Ok(None);
    }
    if persisted.nova_version != nova_core::NOVA_VERSION {
        return Ok(None);
    }

    Ok(Some(persisted.payload))
}
