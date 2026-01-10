use crate::error::CacheError;
use crate::fingerprint::Fingerprint;
use crate::util::{atomic_write, now_millis};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const DERIVED_CACHE_SCHEMA_VERSION: u32 = 1;

/// A best-effort persistent cache for "derived artifacts" (query results).
///
/// This is intentionally separate from any salsa-backed query system; callers
/// provide the query name, arguments, and input fingerprints that should drive
/// invalidation.
#[derive(Clone, Debug)]
pub struct DerivedArtifactCache {
    root: PathBuf,
}

impl DerivedArtifactCache {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }

    pub fn store<T: Serialize>(
        &self,
        query_name: &str,
        args: &impl Serialize,
        input_fingerprints: &BTreeMap<String, Fingerprint>,
        value: &T,
    ) -> Result<(), CacheError> {
        let (path, key_fingerprint) = self.entry_path(query_name, args, input_fingerprints)?;
        let persisted = PersistedDerivedValue {
            schema_version: DERIVED_CACHE_SCHEMA_VERSION,
            nova_version: nova_core::NOVA_VERSION.to_string(),
            saved_at_millis: now_millis(),
            query_name: query_name.to_string(),
            key_fingerprint,
            value,
        };

        let bytes = bincode::serialize(&persisted)?;
        atomic_write(&path, &bytes)
    }

    pub fn load<T: DeserializeOwned>(
        &self,
        query_name: &str,
        args: &impl Serialize,
        input_fingerprints: &BTreeMap<String, Fingerprint>,
    ) -> Result<Option<T>, CacheError> {
        let (path, key_fingerprint) = self.entry_path(query_name, args, input_fingerprints)?;
        if !path.exists() {
            return Ok(None);
        }

        let bytes = std::fs::read(path)?;
        let persisted: PersistedDerivedValueOwned<T> = bincode::deserialize(&bytes)?;

        if persisted.schema_version != DERIVED_CACHE_SCHEMA_VERSION {
            return Ok(None);
        }
        if persisted.nova_version != nova_core::NOVA_VERSION {
            return Ok(None);
        }
        if persisted.query_name != query_name {
            return Ok(None);
        }
        if persisted.key_fingerprint != key_fingerprint {
            return Ok(None);
        }

        Ok(Some(persisted.value))
    }

    fn entry_path(
        &self,
        query_name: &str,
        args: &impl Serialize,
        input_fingerprints: &BTreeMap<String, Fingerprint>,
    ) -> Result<(PathBuf, Fingerprint), CacheError> {
        let safe_query = sanitize_component(query_name);
        let query_dir = self.root.join(safe_query);
        std::fs::create_dir_all(&query_dir)?;

        let args_json = serde_json::to_vec(args)?;
        let inputs_json = serde_json::to_vec(input_fingerprints)?;

        let mut key_bytes = Vec::new();
        key_bytes.extend_from_slice(query_name.as_bytes());
        key_bytes.push(0);
        key_bytes.extend_from_slice(&args_json);
        key_bytes.push(0);
        key_bytes.extend_from_slice(&inputs_json);

        let fingerprint = Fingerprint::from_bytes(key_bytes);
        let path = query_dir.join(format!("{}.bin", fingerprint.as_str()));
        Ok((path, fingerprint))
    }
}

#[derive(Debug, Serialize)]
struct PersistedDerivedValue<'a, T: Serialize> {
    schema_version: u32,
    nova_version: String,
    saved_at_millis: u64,
    query_name: String,
    key_fingerprint: Fingerprint,
    value: &'a T,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedDerivedValueOwned<T> {
    schema_version: u32,
    nova_version: String,
    saved_at_millis: u64,
    query_name: String,
    key_fingerprint: Fingerprint,
    value: T,
}

fn sanitize_component(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect()
}
