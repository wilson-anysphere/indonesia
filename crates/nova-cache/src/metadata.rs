use crate::error::CacheError;
use crate::fingerprint::{Fingerprint, ProjectSnapshot};
use crate::util::now_millis;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;

pub const CACHE_METADATA_SCHEMA_VERSION: u32 = 2;

/// Versioned, per-project cache metadata stored on disk as JSON.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
        let mut invalidated = BTreeSet::new();

        for (path, current_fp) in snapshot.file_fingerprints() {
            match self.file_fingerprints.get(path) {
                Some(previous_fp) if previous_fp == current_fp => {}
                _ => {
                    invalidated.insert(path.clone());
                }
            }
        }

        for path in self.file_fingerprints.keys() {
            if !snapshot.file_fingerprints().contains_key(path) {
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
        let bytes = std::fs::read(path)?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), CacheError> {
        let json = serde_json::to_vec_pretty(self)?;
        crate::util::atomic_write(path.as_ref(), &json)
    }
}

fn compute_metadata_fingerprints(snapshot: &ProjectSnapshot) -> BTreeMap<String, Fingerprint> {
    let mut fingerprints = BTreeMap::new();
    for path in snapshot.file_fingerprints().keys() {
        let full_path = snapshot.project_root().join(path);
        if let Ok(fp) = Fingerprint::from_file_metadata(full_path) {
            fingerprints.insert(path.clone(), fp);
        }
    }
    fingerprints
}
