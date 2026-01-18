use crate::error::CacheError;
use crate::fingerprint::{Fingerprint, ProjectSnapshot};
use crate::util::{atomic_write_with, now_millis};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::OnceLock;

pub const CACHE_METADATA_SCHEMA_VERSION: u32 = 2;
pub const CACHE_METADATA_JSON_FILENAME: &str = "metadata.json";
pub const CACHE_METADATA_BIN_FILENAME: &str = "metadata.bin";

/// Versioned, per-project cache metadata persisted on disk.
///
/// We store both:
/// - `metadata.bin`: `nova-storage` header + `rkyv` archive (fast warm-start path)
/// - `metadata.json`: human-readable debug artifact
#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[archive(check_bytes)]
pub struct CacheMetadata {
    pub schema_version: u32,
    pub nova_version: String,
    pub created_at_millis: u64,
    pub last_updated_millis: u64,
    pub project_hash: Fingerprint,
    pub file_fingerprints: BTreeMap<String, Fingerprint>,
    #[serde(default)]
    pub file_metadata_fingerprints: BTreeMap<String, Fingerprint>,
}

impl CacheMetadata {
    pub fn new(snapshot: &ProjectSnapshot) -> Self {
        let now = now_millis();
        Self {
            schema_version: CACHE_METADATA_SCHEMA_VERSION,
            nova_version: nova_core::NOVA_VERSION.to_string(),
            created_at_millis: now,
            last_updated_millis: now,
            project_hash: snapshot.project_hash().clone(),
            file_fingerprints: snapshot.file_fingerprints().clone(),
            file_metadata_fingerprints: compute_metadata_fingerprints(snapshot),
        }
    }

    pub fn update_from_snapshot(&mut self, snapshot: &ProjectSnapshot) {
        self.last_updated_millis = now_millis();
        self.project_hash = snapshot.project_hash().clone();
        self.file_fingerprints = snapshot.file_fingerprints().clone();
        self.file_metadata_fingerprints = compute_metadata_fingerprints(snapshot);
    }

    /// Compute the set of files that should be invalidated when moving from this
    /// metadata to `snapshot`.
    ///
    /// A file is considered invalidated if it:
    /// - is new (present in `snapshot` but absent from `self.file_fingerprints`)
    /// - is modified (fingerprint differs)
    /// - is deleted (present in `self.file_fingerprints` but absent from `snapshot`)
    pub fn diff_files(&self, snapshot: &ProjectSnapshot) -> Vec<String> {
        self.diff_file_fingerprints(snapshot.file_fingerprints())
    }

    /// Compute the set of invalidated files given a map of current file fingerprints.
    ///
    /// This is equivalent to [`Self::diff_files`] but allows callers to supply
    /// fingerprints computed outside `nova-cache` (e.g. Salsa-derived fingerprints
    /// from a VFS).
    pub fn diff_file_fingerprints(&self, current: &BTreeMap<String, Fingerprint>) -> Vec<String> {
        let mut invalidated = BTreeSet::new();

        for (path, current_fp) in current {
            match self.file_fingerprints.get(path) {
                Some(previous_fp) if previous_fp == current_fp => {}
                _ => {
                    invalidated.insert(path.clone());
                }
            }
        }

        for path in self.file_fingerprints.keys() {
            if !current.contains_key(path) {
                invalidated.insert(path.clone());
            }
        }

        invalidated.into_iter().collect()
    }

    /// Compute the set of files that should be invalidated using "fast"
    /// fingerprints based on file metadata (size + mtime).
    ///
    /// This is intended for warm-start cache validation when callers want to
    /// avoid reading the full contents of every file. It is best-effort:
    /// modifications that preserve both file size and mtime may be missed.
    pub fn diff_files_fast(&self, fast_snapshot: &ProjectSnapshot) -> Vec<String> {
        let mut invalidated = BTreeSet::new();

        for (path, current_fp) in fast_snapshot.file_fingerprints() {
            match self.file_metadata_fingerprints.get(path) {
                Some(previous_fp) if previous_fp == current_fp => {}
                _ => {
                    invalidated.insert(path.clone());
                }
            }
        }

        for path in self.file_metadata_fingerprints.keys() {
            if !fast_snapshot.file_fingerprints().contains_key(path) {
                invalidated.insert(path.clone());
            }
        }

        invalidated.into_iter().collect()
    }

    pub fn is_compatible(&self) -> bool {
        self.schema_version == CACHE_METADATA_SCHEMA_VERSION
            && self.nova_version == nova_core::NOVA_VERSION
    }

    pub fn ensure_compatible(&self) -> Result<(), CacheError> {
        if self.schema_version != CACHE_METADATA_SCHEMA_VERSION {
            return Err(CacheError::IncompatibleSchemaVersion {
                expected: CACHE_METADATA_SCHEMA_VERSION,
                found: self.schema_version,
            });
        }

        if self.nova_version != nova_core::NOVA_VERSION {
            return Err(CacheError::IncompatibleNovaVersion {
                expected: nova_core::NOVA_VERSION.to_string(),
                found: self.nova_version.clone(),
            });
        }

        Ok(())
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, CacheError> {
        let path = path.as_ref();
        let (bin_path, json_path) = metadata_paths(path);

        // Prefer the binary metadata (mmap + rkyv validation). If anything goes
        // wrong, fall back to JSON for robustness.
        if let Ok(Some(archive)) = nova_storage::PersistedArchive::<CacheMetadata>::open_optional(
            &bin_path,
            nova_storage::ArtifactKind::ProjectMetadata,
            CACHE_METADATA_SCHEMA_VERSION,
        ) {
            if let Ok(value) = archive.to_owned() {
                return Ok(value);
            }
        }

        let file = std::fs::File::open(json_path)?;
        Ok(serde_json::from_reader(file)?)
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), CacheError> {
        let path = path.as_ref();
        let (bin_path, json_path) = metadata_paths(path);

        nova_storage::write_archive_atomic(
            &bin_path,
            nova_storage::ArtifactKind::ProjectMetadata,
            self.schema_version,
            self,
            nova_storage::Compression::None,
        )?;

        // Keep a JSON copy around for debugging / human inspection. Avoid pretty
        // printing to keep file size down on large workspaces.
        atomic_write_with(&json_path, |file| {
            serde_json::to_writer(file, self)?;
            Ok(())
        })
    }
}
fn compute_metadata_fingerprints(snapshot: &ProjectSnapshot) -> BTreeMap<String, Fingerprint> {
    static METADATA_FINGERPRINT_ERROR_LOGGED: OnceLock<()> = OnceLock::new();

    let mut fingerprints = BTreeMap::new();
    for (path, content_fingerprint) in snapshot.file_fingerprints() {
        let full_path = snapshot.project_root().join(path);
        let fingerprint = match Fingerprint::from_file_metadata(&full_path) {
            Ok(fp) => fp,
            Err(err) => {
                match &err {
                    CacheError::Io(io_err) if io_err.kind() == std::io::ErrorKind::NotFound => {}
                    _ => {
                        if METADATA_FINGERPRINT_ERROR_LOGGED.set(()).is_ok() {
                            tracing::debug!(
                                target = "nova.cache",
                                path = %full_path.display(),
                                error = %err,
                                "failed to fingerprint file by mtime/size for cache metadata; falling back to content fingerprint"
                            );
                        }
                    }
                }
                content_fingerprint.clone()
            }
        };
        fingerprints.insert(path.clone(), fingerprint);
    }
    fingerprints
}

fn metadata_paths(path: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("bin") | Some("rkyv") => (path.to_path_buf(), path.with_extension("json")),
        Some("json") => (path.with_extension("bin"), path.to_path_buf()),
        _ => (path.with_extension("bin"), path.to_path_buf()),
    }
}

/// A zero-copy view of persisted metadata backed by a validated `rkyv` archive.
///
/// This is intended for warm-start cache validation: callers can compute file
/// invalidation sets without allocating the full `CacheMetadata` (which can be
/// very large for big workspaces).
#[derive(Debug)]
pub struct CacheMetadataArchive {
    archive: nova_storage::PersistedArchive<CacheMetadata>,
}

impl CacheMetadataArchive {
    /// Open `metadata.bin` if present and valid; returns `Ok(None)` on any failure.
    pub fn open(path: impl AsRef<Path>) -> Result<Option<Self>, CacheError> {
        let path = path.as_ref();
        let (bin_path, _json_path) = metadata_paths(path);

        match nova_storage::PersistedArchive::<CacheMetadata>::open_optional(
            &bin_path,
            nova_storage::ArtifactKind::ProjectMetadata,
            CACHE_METADATA_SCHEMA_VERSION,
        ) {
            Ok(Some(archive)) => Ok(Some(Self { archive })),
            Ok(None) => Ok(None),
            Err(_) => Ok(None),
        }
    }

    pub fn is_compatible(&self) -> bool {
        let archived = self.archive.archived();
        archived.schema_version == CACHE_METADATA_SCHEMA_VERSION
            && archived.nova_version.as_str() == nova_core::NOVA_VERSION
    }

    pub fn project_hash(&self) -> &str {
        self.archive.archived().project_hash.as_str()
    }

    pub fn schema_version(&self) -> u32 {
        self.archive.archived().schema_version
    }

    pub fn nova_version(&self) -> &str {
        self.archive.archived().nova_version.as_str()
    }

    pub fn last_updated_millis(&self) -> u64 {
        self.archive.archived().last_updated_millis
    }

    pub fn diff_files(&self, snapshot: &ProjectSnapshot) -> Vec<String> {
        let mut invalidated = BTreeSet::new();
        let stored = &self.archive.archived().file_fingerprints;

        for (path, current_fp) in snapshot.file_fingerprints() {
            match stored.get(path.as_str()) {
                Some(previous_fp) if previous_fp.as_str() == current_fp.as_str() => {}
                _ => {
                    invalidated.insert(path.clone());
                }
            }
        }

        for (path, _) in stored.iter() {
            let path_str = path.as_str();
            if !snapshot.file_fingerprints().contains_key(path_str) {
                invalidated.insert(path_str.to_string());
            }
        }

        invalidated.into_iter().collect()
    }

    pub fn diff_files_fast(&self, snapshot: &ProjectSnapshot) -> Vec<String> {
        let mut invalidated = BTreeSet::new();
        let stored = &self.archive.archived().file_metadata_fingerprints;

        for (path, current_fp) in snapshot.file_fingerprints() {
            match stored.get(path.as_str()) {
                Some(previous_fp) if previous_fp.as_str() == current_fp.as_str() => {}
                _ => {
                    invalidated.insert(path.clone());
                }
            }
        }

        for (path, _) in stored.iter() {
            let path_str = path.as_str();
            if !snapshot.file_fingerprints().contains_key(path_str) {
                invalidated.insert(path_str.to_string());
            }
        }

        invalidated.into_iter().collect()
    }
}
