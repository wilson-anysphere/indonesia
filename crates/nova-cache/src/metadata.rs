use crate::error::CacheError;
use crate::fingerprint::{Fingerprint, ProjectSnapshot};
use crate::util::now_millis;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

pub const CACHE_METADATA_SCHEMA_VERSION: u32 = 1;

/// Versioned, per-project cache metadata stored on disk as JSON.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheMetadata {
    pub schema_version: u32,
    pub nova_version: String,
    pub created_at_millis: u64,
    pub last_updated_millis: u64,
    pub project_hash: Fingerprint,
    pub file_fingerprints: BTreeMap<String, Fingerprint>,
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
        }
    }

    pub fn update_from_snapshot(&mut self, snapshot: &ProjectSnapshot) {
        self.last_updated_millis = now_millis();
        self.project_hash = snapshot.project_hash().clone();
        self.file_fingerprints = snapshot.file_fingerprints().clone();
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
