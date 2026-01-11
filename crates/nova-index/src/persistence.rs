use crate::indexes::{
    AnnotationIndex, ArchivedAnnotationLocation, ArchivedReferenceLocation, ArchivedSymbolLocation,
    InheritanceIndex, ProjectIndexes, ReferenceIndex, SymbolIndex,
};
use nova_cache::{CacheDir, CacheMetadata, ProjectSnapshot};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub const INDEX_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum IndexPersistenceError {
    #[error(transparent)]
    Cache(#[from] nova_cache::CacheError),

    #[error(transparent)]
    Storage(#[from] nova_storage::StorageError),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Clone, Debug)]
pub struct LoadedIndexes {
    pub indexes: ProjectIndexes,
    pub invalidated_files: Vec<String>,
}

#[derive(Debug)]
pub struct LoadedIndexArchives {
    pub symbols: nova_storage::PersistedArchive<SymbolIndex>,
    pub references: nova_storage::PersistedArchive<ReferenceIndex>,
    pub inheritance: nova_storage::PersistedArchive<InheritanceIndex>,
    pub annotations: nova_storage::PersistedArchive<AnnotationIndex>,
    pub invalidated_files: Vec<String>,
}

/// A zero-copy, mmap-backed view over persisted project indexes.
///
/// This is intended for warm-start queries that can operate directly on the
/// archived representation without allocating a full `ProjectIndexes` in memory.
///
/// The view also tracks `invalidated_files` (based on the current
/// [`ProjectSnapshot`]) and filters out results coming from those files so
/// callers see an effectively "pruned" index without requiring
/// `PersistedArchive::to_owned()`.
#[derive(Debug)]
pub struct ProjectIndexesView {
    pub symbols: nova_storage::PersistedArchive<SymbolIndex>,
    pub references: nova_storage::PersistedArchive<ReferenceIndex>,
    pub inheritance: nova_storage::PersistedArchive<InheritanceIndex>,
    pub annotations: nova_storage::PersistedArchive<AnnotationIndex>,

    /// Files whose contents differ from the snapshot used to persist the
    /// indexes (new/modified/deleted).
    pub invalidated_files: BTreeSet<String>,

    /// Optional in-memory overlay for newly indexed/updated files.
    ///
    /// Callers can keep this empty if they only need read-only access to the
    /// persisted archives.
    pub overlay: ProjectIndexes,
}

impl ProjectIndexesView {
    /// Returns `true` if `file` should be treated as stale and filtered out of
    /// archived query results.
    #[inline]
    pub fn is_file_invalidated(&self, file: &str) -> bool {
        self.invalidated_files.contains(file)
    }

    /// Returns all symbol names that have at least one location in a
    /// non-invalidated file.
    pub fn symbol_names<'a>(&'a self) -> impl Iterator<Item = &'a str> + 'a {
        let invalidated_files = &self.invalidated_files;
        self.symbols
            .archived()
            .symbols
            .iter()
            .filter_map(move |(name, locations)| {
                locations
                    .iter()
                    .any(|loc| !invalidated_files.contains(loc.file.as_str()))
                    .then(|| name.as_str())
            })
    }

    /// Returns symbol definition locations for `name`, filtering out any
    /// locations that come from invalidated files.
    pub fn symbol_locations<'a>(
        &'a self,
        name: &str,
    ) -> impl Iterator<Item = &'a ArchivedSymbolLocation> + 'a {
        let invalidated_files = &self.invalidated_files;
        self.symbols
            .archived()
            .symbols
            .get(name)
            .into_iter()
            .flat_map(move |locations| {
                locations
                    .iter()
                    .filter(move |loc| !invalidated_files.contains(loc.file.as_str()))
            })
    }

    /// Returns annotation locations for `name`, filtering out any locations
    /// that come from invalidated files.
    pub fn annotation_locations<'a>(
        &'a self,
        name: &str,
    ) -> impl Iterator<Item = &'a ArchivedAnnotationLocation> + 'a {
        let invalidated_files = &self.invalidated_files;
        self.annotations
            .archived()
            .annotations
            .get(name)
            .into_iter()
            .flat_map(move |locations| {
                locations
                    .iter()
                    .filter(move |loc| !invalidated_files.contains(loc.file.as_str()))
            })
    }

    /// Returns all annotation names that have at least one location in a
    /// non-invalidated file.
    pub fn annotation_names<'a>(&'a self) -> impl Iterator<Item = &'a str> + 'a {
        let invalidated_files = &self.invalidated_files;
        self.annotations
            .archived()
            .annotations
            .iter()
            .filter_map(move |(name, locations)| {
                locations
                    .iter()
                    .any(|loc| !invalidated_files.contains(loc.file.as_str()))
                    .then(|| name.as_str())
            })
    }

    /// Returns reference locations for `symbol`, filtering out any locations
    /// that come from invalidated files.
    pub fn reference_locations<'a>(
        &'a self,
        symbol: &str,
    ) -> impl Iterator<Item = &'a ArchivedReferenceLocation> + 'a {
        let invalidated_files = &self.invalidated_files;
        self.references
            .archived()
            .references
            .get(symbol)
            .into_iter()
            .flat_map(move |locations| {
                locations
                    .iter()
                    .filter(move |loc| !invalidated_files.contains(loc.file.as_str()))
            })
    }

    /// Returns all symbols that have at least one reference location in a
    /// non-invalidated file.
    pub fn referenced_symbols<'a>(&'a self) -> impl Iterator<Item = &'a str> + 'a {
        let invalidated_files = &self.invalidated_files;
        self.references
            .archived()
            .references
            .iter()
            .filter_map(move |(symbol, locations)| {
                locations
                    .iter()
                    .any(|loc| !invalidated_files.contains(loc.file.as_str()))
                    .then(|| symbol.as_str())
            })
    }
}

pub fn save_indexes(
    cache_dir: &CacheDir,
    snapshot: &ProjectSnapshot,
    indexes: &ProjectIndexes,
) -> Result<(), IndexPersistenceError> {
    let indexes_dir = cache_dir.indexes_dir();
    std::fs::create_dir_all(&indexes_dir)?;

    write_index_file(
        indexes_dir.join("symbols.idx"),
        nova_storage::ArtifactKind::SymbolIndex,
        &indexes.symbols,
    )?;
    write_index_file(
        indexes_dir.join("references.idx"),
        nova_storage::ArtifactKind::ReferenceIndex,
        &indexes.references,
    )?;
    write_index_file(
        indexes_dir.join("inheritance.idx"),
        nova_storage::ArtifactKind::InheritanceIndex,
        &indexes.inheritance,
    )?;
    write_index_file(
        indexes_dir.join("annotations.idx"),
        nova_storage::ArtifactKind::AnnotationIndex,
        &indexes.annotations,
    )?;

    let metadata_path = cache_dir.metadata_path();
    let mut metadata = match CacheMetadata::load(&metadata_path) {
        Ok(existing)
            if existing.is_compatible() && &existing.project_hash == snapshot.project_hash() =>
        {
            existing
        }
        _ => CacheMetadata::new(snapshot),
    };
    metadata.update_from_snapshot(snapshot);
    metadata.save(metadata_path)?;
    Ok(())
}

/// Loads indexes as validated `rkyv` archives backed by an mmap when possible.
///
/// Callers that require an owned, mutable `ProjectIndexes` should use
/// [`load_indexes`]. This function is intended for warm-start queries where the
/// archived representation can be queried without allocating an owned copy.
pub fn load_index_archives(
    cache_dir: &CacheDir,
    current_snapshot: &ProjectSnapshot,
) -> Result<Option<LoadedIndexArchives>, IndexPersistenceError> {
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

    let symbols = match open_index_file::<SymbolIndex>(
        indexes_dir.join("symbols.idx"),
        nova_storage::ArtifactKind::SymbolIndex,
    ) {
        Some(value) => value,
        None => return Ok(None),
    };
    let references = match open_index_file::<ReferenceIndex>(
        indexes_dir.join("references.idx"),
        nova_storage::ArtifactKind::ReferenceIndex,
    ) {
        Some(value) => value,
        None => return Ok(None),
    };
    let inheritance = match open_index_file::<InheritanceIndex>(
        indexes_dir.join("inheritance.idx"),
        nova_storage::ArtifactKind::InheritanceIndex,
    ) {
        Some(value) => value,
        None => return Ok(None),
    };
    let annotations = match open_index_file::<AnnotationIndex>(
        indexes_dir.join("annotations.idx"),
        nova_storage::ArtifactKind::AnnotationIndex,
    ) {
        Some(value) => value,
        None => return Ok(None),
    };

    let invalidated = metadata.diff_files(current_snapshot);

    Ok(Some(LoadedIndexArchives {
        symbols,
        references,
        inheritance,
        annotations,
        invalidated_files: invalidated,
    }))
}

/// Loads indexes as validated `rkyv` archives backed by an mmap when possible,
/// using a fast per-file fingerprint based on file metadata (size + mtime).
///
/// This avoids hashing full file contents before deciding whether persisted
/// indexes are reusable. It is best-effort: modifications that preserve both
/// file size and mtime may be missed.
pub fn load_index_archives_fast(
    cache_dir: &CacheDir,
    project_root: impl AsRef<Path>,
    files: Vec<PathBuf>,
) -> Result<Option<LoadedIndexArchives>, IndexPersistenceError> {
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

    let current_snapshot = match ProjectSnapshot::new_fast(project_root, files) {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };

    if &metadata.project_hash != current_snapshot.project_hash() {
        return Ok(None);
    }

    let indexes_dir = cache_dir.indexes_dir();

    let symbols = match open_index_file::<SymbolIndex>(
        indexes_dir.join("symbols.idx"),
        nova_storage::ArtifactKind::SymbolIndex,
    ) {
        Some(value) => value,
        None => return Ok(None),
    };
    let references = match open_index_file::<ReferenceIndex>(
        indexes_dir.join("references.idx"),
        nova_storage::ArtifactKind::ReferenceIndex,
    ) {
        Some(value) => value,
        None => return Ok(None),
    };
    let inheritance = match open_index_file::<InheritanceIndex>(
        indexes_dir.join("inheritance.idx"),
        nova_storage::ArtifactKind::InheritanceIndex,
    ) {
        Some(value) => value,
        None => return Ok(None),
    };
    let annotations = match open_index_file::<AnnotationIndex>(
        indexes_dir.join("annotations.idx"),
        nova_storage::ArtifactKind::AnnotationIndex,
    ) {
        Some(value) => value,
        None => return Ok(None),
    };

    let invalidated = metadata.diff_files_fast(&current_snapshot);

    Ok(Some(LoadedIndexArchives {
        symbols,
        references,
        inheritance,
        annotations,
        invalidated_files: invalidated,
    }))
}

pub fn load_indexes(
    cache_dir: &CacheDir,
    current_snapshot: &ProjectSnapshot,
) -> Result<Option<LoadedIndexes>, IndexPersistenceError> {
    let Some(archives) = load_index_archives(cache_dir, current_snapshot)? else {
        return Ok(None);
    };

    let symbols = match archives.symbols.to_owned() {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    let references = match archives.references.to_owned() {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    let inheritance = match archives.inheritance.to_owned() {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    let annotations = match archives.annotations.to_owned() {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };

    let mut indexes = ProjectIndexes {
        symbols,
        references,
        inheritance,
        annotations,
    };

    for file in &archives.invalidated_files {
        indexes.invalidate_file(file);
    }

    Ok(Some(LoadedIndexes {
        indexes,
        invalidated_files: archives.invalidated_files,
    }))
}

/// Loads indexes as a zero-copy view backed by validated `rkyv` archives.
///
/// This is similar to [`load_indexes`], but avoids deserializing the full
/// `ProjectIndexes` into memory. Instead, callers can query the persisted
/// archives directly via helper methods on [`ProjectIndexesView`].
pub fn load_index_view(
    cache_dir: &CacheDir,
    current_snapshot: &ProjectSnapshot,
) -> Result<Option<ProjectIndexesView>, IndexPersistenceError> {
    let Some(archives) = load_index_archives(cache_dir, current_snapshot)? else {
        return Ok(None);
    };

    let invalidated_files = archives
        .invalidated_files
        .into_iter()
        .collect::<BTreeSet<_>>();

    Ok(Some(ProjectIndexesView {
        symbols: archives.symbols,
        references: archives.references,
        inheritance: archives.inheritance,
        annotations: archives.annotations,
        invalidated_files,
        overlay: ProjectIndexes::default(),
    }))
}

pub fn load_indexes_fast(
    cache_dir: &CacheDir,
    project_root: impl AsRef<Path>,
    files: Vec<PathBuf>,
) -> Result<Option<LoadedIndexes>, IndexPersistenceError> {
    let Some(archives) = load_index_archives_fast(cache_dir, project_root, files)? else {
        return Ok(None);
    };

    let symbols = match archives.symbols.to_owned() {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    let references = match archives.references.to_owned() {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    let inheritance = match archives.inheritance.to_owned() {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    let annotations = match archives.annotations.to_owned() {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };

    let mut indexes = ProjectIndexes {
        symbols,
        references,
        inheritance,
        annotations,
    };

    for file in &archives.invalidated_files {
        indexes.invalidate_file(file);
    }

    Ok(Some(LoadedIndexes {
        indexes,
        invalidated_files: archives.invalidated_files,
    }))
}

fn write_index_file<T>(
    path: PathBuf,
    kind: nova_storage::ArtifactKind,
    payload: &T,
) -> Result<(), IndexPersistenceError>
where
    T: rkyv::Archive + rkyv::Serialize<rkyv::ser::serializers::AllocSerializer<256>>,
{
    nova_storage::write_archive_atomic(
        &path,
        kind,
        INDEX_SCHEMA_VERSION,
        payload,
        nova_storage::Compression::None,
    )?;
    Ok(())
}

fn open_index_file<T>(
    path: PathBuf,
    kind: nova_storage::ArtifactKind,
) -> Option<nova_storage::PersistedArchive<T>>
where
    T: rkyv::Archive,
    rkyv::Archived<T>: nova_storage::CheckableArchived,
{
    match nova_storage::PersistedArchive::<T>::open_optional(&path, kind, INDEX_SCHEMA_VERSION) {
        Ok(value) => value,
        Err(_) => None,
    }
}
