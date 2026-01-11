use crate::indexes::{
    AnnotationIndex, AnnotationLocation, ArchivedAnnotationLocation, ArchivedReferenceLocation,
    ArchivedSymbolLocation, InheritanceIndex, ProjectIndexes, ReferenceIndex, ReferenceLocation,
    SymbolIndex, SymbolLocation,
};
use nova_cache::{CacheDir, CacheMetadata, ProjectSnapshot};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub const INDEX_SCHEMA_VERSION: u32 = 1;

pub const DEFAULT_SHARD_COUNT: u32 = 64;

pub type ShardId = u32;

#[derive(Debug, thiserror::Error)]
pub enum IndexPersistenceError {
    #[error(transparent)]
    Cache(#[from] nova_cache::CacheError),

    #[error(transparent)]
    Storage(#[from] nova_storage::StorageError),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("invalid shard count {shard_count}")]
    InvalidShardCount { shard_count: u32 },

    #[error("shard vector length mismatch: expected {expected}, got {found}")]
    ShardVectorLenMismatch { expected: usize, found: usize },
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

/// A lightweight, allocation-free view of a location stored in either a
/// persisted archive or the in-memory overlay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocationRef<'a> {
    pub file: &'a str,
    pub line: u32,
    pub column: u32,
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

    /// Returns symbol definition locations for `name`, merging persisted results
    /// (with invalidated files filtered out) and the in-memory overlay.
    ///
    /// This is useful for incremental indexing flows where callers want to
    /// query updated files (stored in `overlay`) without materializing the full
    /// persisted index into memory.
    pub fn symbol_locations_merged<'a>(
        &'a self,
        name: &str,
    ) -> impl Iterator<Item = LocationRef<'a>> + 'a {
        let archived = self.symbol_locations(name).map(|loc| LocationRef {
            file: loc.file.as_str(),
            line: loc.line,
            column: loc.column,
        });

        let overlay = self
            .overlay
            .symbols
            .symbols
            .get(name)
            .into_iter()
            .flat_map(|locations| locations.iter())
            .map(|loc| LocationRef {
                file: loc.file.as_str(),
                line: loc.line,
                column: loc.column,
            });

        archived.chain(overlay)
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

    /// Returns annotation locations for `name`, merging persisted results (with
    /// invalidated files filtered out) and the in-memory overlay.
    pub fn annotation_locations_merged<'a>(
        &'a self,
        name: &str,
    ) -> impl Iterator<Item = LocationRef<'a>> + 'a {
        let archived = self.annotation_locations(name).map(|loc| LocationRef {
            file: loc.file.as_str(),
            line: loc.line,
            column: loc.column,
        });

        let overlay = self
            .overlay
            .annotations
            .annotations
            .get(name)
            .into_iter()
            .flat_map(|locations| locations.iter())
            .map(|loc| LocationRef {
                file: loc.file.as_str(),
                line: loc.line,
                column: loc.column,
            });

        archived.chain(overlay)
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

    /// Returns reference locations for `symbol`, merging persisted results
    /// (with invalidated files filtered out) and the in-memory overlay.
    pub fn reference_locations_merged<'a>(
        &'a self,
        symbol: &str,
    ) -> impl Iterator<Item = LocationRef<'a>> + 'a {
        let archived = self.reference_locations(symbol).map(|loc| LocationRef {
            file: loc.file.as_str(),
            line: loc.line,
            column: loc.column,
        });

        let overlay = self
            .overlay
            .references
            .references
            .get(symbol)
            .into_iter()
            .flat_map(|locations| locations.iter())
            .map(|loc| LocationRef {
                file: loc.file.as_str(),
                line: loc.line,
                column: loc.column,
            });

        archived.chain(overlay)
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

/// Loads indexes as a zero-copy view backed by validated `rkyv` archives, using
/// a fast per-file fingerprint based on file metadata (size + mtime).
///
/// This avoids hashing full file contents before deciding whether persisted
/// indexes are reusable. It is best-effort: modifications that preserve both
/// file size and mtime may be missed.
pub fn load_index_view_fast(
    cache_dir: &CacheDir,
    project_root: impl AsRef<Path>,
    files: Vec<PathBuf>,
) -> Result<Option<ProjectIndexesView>, IndexPersistenceError> {
    let Some(archives) = load_index_archives_fast(cache_dir, project_root, files)? else {
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
    nova_storage::PersistedArchive::<T>::open_optional(&path, kind, INDEX_SCHEMA_VERSION)
        .unwrap_or_default()
}

// ---------------------------------------------------------------------
// Sharded persistence (incremental, per-shard archives)
// ---------------------------------------------------------------------

const SHARDS_DIR_NAME: &str = "shards";
const SHARD_MANIFEST_FILE: &str = "manifest.txt";

#[derive(Debug)]
pub struct LoadedShardIndexArchives {
    pub symbols: nova_storage::PersistedArchive<SymbolIndex>,
    pub references: nova_storage::PersistedArchive<ReferenceIndex>,
    pub inheritance: nova_storage::PersistedArchive<InheritanceIndex>,
    pub annotations: nova_storage::PersistedArchive<AnnotationIndex>,
}

#[derive(Debug)]
pub struct LoadedShardedIndexArchives {
    /// Per-shard persisted archives. `None` indicates the shard is missing or corrupt.
    pub shards: Vec<Option<LoadedShardIndexArchives>>,
    /// Files that should be re-indexed in the current snapshot.
    pub invalidated_files: Vec<String>,
    /// Shards that are missing or corrupt and therefore must be rebuilt.
    pub missing_shards: BTreeSet<ShardId>,
}

#[derive(Debug)]
pub struct LoadedShardedIndexView {
    pub view: ShardedIndexView,
    pub invalidated_files: Vec<String>,
    pub missing_shards: BTreeSet<ShardId>,
}

/// Query interface over sharded, persisted indexes.
///
/// This type is intentionally read-only: it operates directly on the archived `rkyv`
/// representation backed by `mmap` where possible.
#[derive(Debug)]
pub struct ShardedIndexView {
    shards: Vec<Option<LoadedShardIndexArchives>>,
}

impl ShardedIndexView {
    #[must_use]
    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }

    #[must_use]
    pub fn shard(&self, shard_id: ShardId) -> Option<&LoadedShardIndexArchives> {
        self.shards.get(shard_id as usize)?.as_ref()
    }

    /// Return all `SymbolLocation`s for `symbol` across all available shards.
    ///
    /// This is a convenience helper for consumers that want a global view without
    /// deserializing the entire index set.
    #[must_use]
    pub fn symbol_locations(&self, symbol: &str) -> Vec<SymbolLocation> {
        let mut out = Vec::new();
        for shard in self.shards.iter().filter_map(|s| s.as_ref()) {
            let Some(locations) = shard.symbols.symbols.get(symbol) else {
                continue;
            };
            out.extend(locations.iter().map(|loc| SymbolLocation {
                file: loc.file.as_str().to_string(),
                line: loc.line,
                column: loc.column,
            }));
        }
        out
    }

    #[must_use]
    pub fn reference_locations(&self, symbol: &str) -> Vec<ReferenceLocation> {
        let mut out = Vec::new();
        for shard in self.shards.iter().filter_map(|s| s.as_ref()) {
            let Some(locations) = shard.references.references.get(symbol) else {
                continue;
            };
            out.extend(locations.iter().map(|loc| ReferenceLocation {
                file: loc.file.as_str().to_string(),
                line: loc.line,
                column: loc.column,
            }));
        }
        out
    }

    #[must_use]
    pub fn annotation_locations(&self, annotation: &str) -> Vec<AnnotationLocation> {
        let mut out = Vec::new();
        for shard in self.shards.iter().filter_map(|s| s.as_ref()) {
            let Some(locations) = shard.annotations.annotations.get(annotation) else {
                continue;
            };
            out.extend(locations.iter().map(|loc| AnnotationLocation {
                file: loc.file.as_str().to_string(),
                line: loc.line,
                column: loc.column,
            }));
        }
        out
    }
}

/// Deterministically map a relative file path to a shard id.
///
/// Sharding is stable across runs for the same `path`/`shard_count` combination.
#[must_use]
pub fn shard_id_for_path(path: &str, shard_count: u32) -> ShardId {
    if shard_count == 0 {
        return 0;
    }

    let hash = blake3::hash(path.as_bytes());
    let mut prefix = [0u8; 8];
    prefix.copy_from_slice(&hash.as_bytes()[..8]);
    let value = u64::from_le_bytes(prefix);
    (value % shard_count as u64) as ShardId
}

#[must_use]
pub fn affected_shards(invalidated_files: &[String], shard_count: u32) -> BTreeSet<ShardId> {
    invalidated_files
        .iter()
        .map(|path| shard_id_for_path(path, shard_count))
        .collect()
}

pub fn save_sharded_indexes(
    cache_dir: &CacheDir,
    snapshot: &ProjectSnapshot,
    shard_count: u32,
    shards: Vec<ProjectIndexes>,
) -> Result<(), IndexPersistenceError> {
    if shard_count == 0 {
        return Err(IndexPersistenceError::InvalidShardCount { shard_count });
    }
    if shards.len() != shard_count as usize {
        return Err(IndexPersistenceError::ShardVectorLenMismatch {
            expected: shard_count as usize,
            found: shards.len(),
        });
    }

    let indexes_dir = cache_dir.indexes_dir();
    let shards_root = indexes_dir.join(SHARDS_DIR_NAME);
    std::fs::create_dir_all(&shards_root)?;

    // Write/update shard manifest so loads can treat shard-count changes as a cache miss.
    write_shard_manifest(&shards_root, shard_count)?;

    // Determine which shards need to be rewritten based on the previous metadata snapshot.
    let metadata_path = cache_dir.metadata_path();
    let previous_metadata = CacheMetadata::load(&metadata_path)
        .ok()
        .filter(|m| m.is_compatible() && &m.project_hash == snapshot.project_hash());

    let mut shards_to_write = match &previous_metadata {
        Some(metadata) => affected_shards(&metadata.diff_files(snapshot), shard_count),
        None => (0..shard_count).collect(),
    };

    // Also rewrite shards that are missing/corrupt on disk (best-effort recovery).
    for shard_id in 0..shard_count {
        if !shard_on_disk_is_healthy(&shards_root, shard_id) {
            shards_to_write.insert(shard_id);
        }
    }

    for shard_id in shards_to_write {
        let shard_dir = shard_dir(&shards_root, shard_id);
        std::fs::create_dir_all(&shard_dir)?;
        let shard = &shards[shard_id as usize];

        write_index_file(
            shard_dir.join("symbols.idx"),
            nova_storage::ArtifactKind::SymbolIndex,
            &shard.symbols,
        )?;
        write_index_file(
            shard_dir.join("references.idx"),
            nova_storage::ArtifactKind::ReferenceIndex,
            &shard.references,
        )?;
        write_index_file(
            shard_dir.join("inheritance.idx"),
            nova_storage::ArtifactKind::InheritanceIndex,
            &shard.inheritance,
        )?;
        write_index_file(
            shard_dir.join("annotations.idx"),
            nova_storage::ArtifactKind::AnnotationIndex,
            &shard.annotations,
        )?;
    }

    // Update metadata after persisting the shards.
    let mut metadata = match previous_metadata {
        Some(existing) => existing,
        None => CacheMetadata::new(snapshot),
    };
    metadata.update_from_snapshot(snapshot);
    metadata.save(metadata_path)?;

    Ok(())
}

/// Load sharded indexes as validated `rkyv` archives backed by an mmap when possible.
///
/// Backwards compatibility:
/// - If `indexes/shards/manifest.txt` is missing, this treats the cache as a miss and does
///   **not** attempt to read legacy monolithic `indexes/symbols.idx` files. Callers should
///   rebuild and persist using the sharded APIs.
pub fn load_sharded_index_archives(
    cache_dir: &CacheDir,
    current_snapshot: &ProjectSnapshot,
    shard_count: u32,
) -> Result<Option<LoadedShardedIndexArchives>, IndexPersistenceError> {
    if shard_count == 0 {
        return Err(IndexPersistenceError::InvalidShardCount { shard_count });
    }

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

    let shards_root = cache_dir.indexes_dir().join(SHARDS_DIR_NAME);
    match read_shard_manifest(&shards_root) {
        Some(value) if value == shard_count => {}
        _ => return Ok(None),
    }

    let mut shards = Vec::with_capacity(shard_count as usize);
    let mut missing_shards = BTreeSet::new();

    for shard_id in 0..shard_count {
        let shard_dir = shard_dir(&shards_root, shard_id);
        let Some(shard_archives) = load_shard_archives(&shard_dir) else {
            shards.push(None);
            missing_shards.insert(shard_id);
            continue;
        };
        shards.push(Some(shard_archives));
    }

    // Base invalidation from snapshot diffs.
    let mut invalidated: BTreeSet<String> =
        metadata.diff_files(current_snapshot).into_iter().collect();

    // If a shard is missing/corrupt, treat all files that map to that shard as invalidated so the
    // caller can rebuild just those shards.
    if !missing_shards.is_empty() {
        for path in current_snapshot.file_fingerprints().keys() {
            if missing_shards.contains(&shard_id_for_path(path, shard_count)) {
                invalidated.insert(path.clone());
            }
        }
    }

    Ok(Some(LoadedShardedIndexArchives {
        shards,
        invalidated_files: invalidated.into_iter().collect(),
        missing_shards,
    }))
}

pub fn load_sharded_index_view(
    cache_dir: &CacheDir,
    current_snapshot: &ProjectSnapshot,
    shard_count: u32,
) -> Result<Option<LoadedShardedIndexView>, IndexPersistenceError> {
    let Some(archives) = load_sharded_index_archives(cache_dir, current_snapshot, shard_count)?
    else {
        return Ok(None);
    };

    Ok(Some(LoadedShardedIndexView {
        view: ShardedIndexView {
            shards: archives.shards,
        },
        invalidated_files: archives.invalidated_files,
        missing_shards: archives.missing_shards,
    }))
}

fn shard_dir(shards_root: &Path, shard_id: ShardId) -> PathBuf {
    shards_root.join(shard_id.to_string())
}

fn shard_manifest_path(shards_root: &Path) -> PathBuf {
    shards_root.join(SHARD_MANIFEST_FILE)
}

fn write_shard_manifest(shards_root: &Path, shard_count: u32) -> Result<(), IndexPersistenceError> {
    let manifest_path = shard_manifest_path(shards_root);
    nova_cache::atomic_write(&manifest_path, format!("{shard_count}\n").as_bytes())?;
    Ok(())
}

fn read_shard_manifest(shards_root: &Path) -> Option<u32> {
    let manifest_path = shard_manifest_path(shards_root);
    let text = std::fs::read_to_string(manifest_path).ok()?;
    let line = text.lines().next()?.trim();
    line.parse::<u32>().ok()
}

fn load_shard_archives(shard_dir: &Path) -> Option<LoadedShardIndexArchives> {
    let symbols = open_index_file::<SymbolIndex>(
        shard_dir.join("symbols.idx"),
        nova_storage::ArtifactKind::SymbolIndex,
    )?;
    let references = open_index_file::<ReferenceIndex>(
        shard_dir.join("references.idx"),
        nova_storage::ArtifactKind::ReferenceIndex,
    )?;
    let inheritance = open_index_file::<InheritanceIndex>(
        shard_dir.join("inheritance.idx"),
        nova_storage::ArtifactKind::InheritanceIndex,
    )?;
    let annotations = open_index_file::<AnnotationIndex>(
        shard_dir.join("annotations.idx"),
        nova_storage::ArtifactKind::AnnotationIndex,
    )?;

    Some(LoadedShardIndexArchives {
        symbols,
        references,
        inheritance,
        annotations,
    })
}

fn shard_on_disk_is_healthy(shards_root: &Path, shard_id: ShardId) -> bool {
    let shard_dir = shard_dir(shards_root, shard_id);
    if !shard_dir.exists() {
        return false;
    }

    // Open each file to ensure the payload validates; this keeps the "missing shard" detection
    // aligned with `load_sharded_index_archives` so incremental rebuild/save logic agrees about
    // which shards are recoverable.
    load_shard_archives(&shard_dir).is_some()
}
