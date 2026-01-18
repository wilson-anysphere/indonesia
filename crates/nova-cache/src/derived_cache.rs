use crate::error::CacheError;
use crate::fingerprint::Fingerprint;
use crate::path::normalize_inputs_map;
use crate::util::{
    atomic_write, atomic_write_with, bincode_deserialize, bincode_options_limited,
    bincode_serialize, now_millis, read_file_limited, remove_file_best_effort,
};
use bincode::Options;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

pub const DERIVED_CACHE_SCHEMA_VERSION: u32 = 2;
const DERIVED_CACHE_INDEX_SCHEMA_VERSION: u32 = 1;
const DERIVED_CACHE_INDEX_FILE_NAME: &str = "index.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DerivedCachePolicy {
    pub max_bytes: u64,
    pub max_age_ms: Option<u64>,
    pub per_query_max_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DerivedCacheQueryStats {
    pub bytes: u64,
    pub entries: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DerivedCacheStats {
    pub total_bytes: u64,
    pub total_entries: u64,
    pub per_query: BTreeMap<String, DerivedCacheQueryStats>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DerivedCacheGcReport {
    pub before: DerivedCacheStats,
    pub after: DerivedCacheStats,
    pub deleted_bytes: u64,
    pub deleted_entries: u64,
}

/// A best-effort persistent cache for "derived artifacts" (query results).
///
/// This is intentionally separate from any salsa-backed query system; callers
/// provide the query name, per-query schema version, arguments, and input
/// fingerprints that should drive invalidation.
///
/// Note: Any file paths that participate in cache keys should be normalized
/// (see [`crate::normalize_rel_path`]) to avoid duplicate entries across
/// platforms/path sources. `input_fingerprints` keys are normalized internally,
/// but callers are responsible for normalizing path-like strings inside `args`.
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

    /// Compute the fingerprint used to key a derived artifact.
    ///
    /// This is exposed so other caching layers (e.g. in-memory query caches) can
    /// share a consistent key with the on-disk [`DerivedArtifactCache`].
    pub fn key_fingerprint(
        query_name: &str,
        query_schema_version: u32,
        args: &impl Serialize,
        input_fingerprints: &BTreeMap<String, Fingerprint>,
    ) -> Result<Fingerprint, CacheError> {
        let args_json = serde_json::to_vec(args)?;
        let normalized_inputs = normalize_inputs_map(input_fingerprints);
        let inputs_json = serde_json::to_vec(&normalized_inputs)?;

        let mut key_bytes = Vec::new();
        key_bytes.extend_from_slice(query_name.as_bytes());
        key_bytes.push(0);
        key_bytes.extend_from_slice(&query_schema_version.to_le_bytes());
        key_bytes.push(0);
        key_bytes.extend_from_slice(&args_json);
        key_bytes.push(0);
        key_bytes.extend_from_slice(&inputs_json);

        Ok(Fingerprint::from_bytes(key_bytes))
    }

    pub fn store<T: Serialize>(
        &self,
        query_name: &str,
        query_schema_version: u32,
        args: &impl Serialize,
        input_fingerprints: &BTreeMap<String, Fingerprint>,
        value: &T,
    ) -> Result<(), CacheError> {
        let (path, key_fingerprint) =
            self.entry_path(query_name, query_schema_version, args, input_fingerprints)?;
        let saved_at_millis = now_millis();
        let persisted = PersistedDerivedValue {
            schema_version: DERIVED_CACHE_SCHEMA_VERSION,
            query_schema_version,
            nova_version: nova_core::NOVA_VERSION.to_string(),
            saved_at_millis,
            query_name: query_name.to_string(),
            key_fingerprint,
            value,
        };

        let bytes = bincode_serialize(&persisted)?;
        atomic_write(&path, &bytes)?;

        let entry_size = bytes.len() as u64;
        let _ = self.update_index_after_store(&path, saved_at_millis, entry_size);
        Ok(())
    }

    pub fn load<T: DeserializeOwned>(
        &self,
        query_name: &str,
        query_schema_version: u32,
        args: &impl Serialize,
        input_fingerprints: &BTreeMap<String, Fingerprint>,
    ) -> Result<Option<T>, CacheError> {
        let (path, key_fingerprint) =
            self.entry_path(query_name, query_schema_version, args, input_fingerprints)?;
        if !path.exists() {
            return Ok(None);
        }

        let bytes = match read_file_limited(&path) {
            Some(bytes) => bytes,
            None => return Ok(None),
        };
        let persisted: PersistedDerivedValueOwned<T> = match bincode_deserialize(&bytes) {
            Ok(value) => value,
            Err(_) => {
                remove_file_best_effort(&path, "derived_cache.decode");
                return Ok(None);
            }
        };

        if persisted.schema_version != DERIVED_CACHE_SCHEMA_VERSION {
            remove_file_best_effort(&path, "derived_cache.schema_version");
            return Ok(None);
        }
        if persisted.query_schema_version != query_schema_version {
            remove_file_best_effort(&path, "derived_cache.query_schema_version");
            return Ok(None);
        }
        if persisted.nova_version != nova_core::NOVA_VERSION {
            remove_file_best_effort(&path, "derived_cache.nova_version");
            return Ok(None);
        }
        if persisted.query_name != query_name {
            remove_file_best_effort(&path, "derived_cache.query_name");
            return Ok(None);
        }
        if persisted.key_fingerprint != key_fingerprint {
            remove_file_best_effort(&path, "derived_cache.key_fingerprint");
            return Ok(None);
        }

        Ok(Some(persisted.value))
    }

    pub fn stats(&self) -> Result<DerivedCacheStats, CacheError> {
        let mut stats = DerivedCacheStats::default();
        for mut query in self.load_query_states()? {
            if query.dirty {
                let _ = self.write_query_index(&query.dir, &query.index);
                query.dirty = false;
            }

            let (bytes, entries) = query.index.totals();
            stats.total_bytes += bytes;
            stats.total_entries += entries;
            stats
                .per_query
                .insert(query.name, DerivedCacheQueryStats { bytes, entries });
        }
        Ok(stats)
    }

    pub fn gc(&self, policy: DerivedCachePolicy) -> Result<DerivedCacheGcReport, CacheError> {
        let mut query_states = self.load_query_states()?;
        let before = compute_stats_for_states(&query_states);

        let now = now_millis();
        let cutoff = policy.max_age_ms.map(|ttl| now.saturating_sub(ttl));

        let mut deleted_bytes = 0u64;
        let mut deleted_entries = 0u64;

        // 1) TTL eviction.
        if let Some(cutoff) = cutoff {
            for query in &mut query_states {
                let candidates: Vec<_> = query
                    .index
                    .entries
                    .iter()
                    .filter_map(|(file_name, meta)| {
                        if meta.saved_at_millis < cutoff {
                            Some((file_name.clone(), meta.size))
                        } else {
                            None
                        }
                    })
                    .collect();
                for (file_name, size) in candidates {
                    if self.delete_entry_file(&query.dir, &file_name)? {
                        query.index.entries.remove(&file_name);
                        query.dirty = true;
                        deleted_bytes += size;
                        deleted_entries += 1;
                    }
                }
            }
        }

        // 2) Per-query budget eviction.
        if let Some(per_query_max_bytes) = policy.per_query_max_bytes {
            for query in &mut query_states {
                let mut bytes = query.index.total_bytes();
                if bytes <= per_query_max_bytes {
                    continue;
                }

                let mut entries: Vec<_> = query
                    .index
                    .entries
                    .iter()
                    .map(|(file_name, meta)| (meta.saved_at_millis, file_name.clone(), meta.size))
                    .collect();
                entries.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));

                for (_saved_at, file_name, size) in entries {
                    if bytes <= per_query_max_bytes {
                        break;
                    }
                    if self.delete_entry_file(&query.dir, &file_name)? {
                        query.index.entries.remove(&file_name);
                        query.dirty = true;
                        bytes = bytes.saturating_sub(size);
                        deleted_bytes += size;
                        deleted_entries += 1;
                    }
                }
            }
        }

        // 3) Global budget eviction.
        let mut total_bytes = query_states
            .iter()
            .map(|q| q.index.total_bytes())
            .sum::<u64>();
        if total_bytes > policy.max_bytes {
            let mut global: Vec<_> = query_states
                .iter()
                .enumerate()
                .flat_map(|(idx, q)| {
                    q.index.entries.iter().map(move |(file_name, meta)| {
                        (
                            meta.saved_at_millis,
                            q.name.clone(),
                            file_name.clone(),
                            meta.size,
                            idx,
                        )
                    })
                })
                .collect();
            global.sort_by(|a, b| (a.0, &a.1, &a.2).cmp(&(b.0, &b.1, &b.2)));

            for (_saved_at, _query_name, file_name, size, idx) in global {
                if total_bytes <= policy.max_bytes {
                    break;
                }
                let query = &mut query_states[idx];
                if !query.index.entries.contains_key(&file_name) {
                    continue;
                }
                if self.delete_entry_file(&query.dir, &file_name)? {
                    query.index.entries.remove(&file_name);
                    query.dirty = true;
                    total_bytes = total_bytes.saturating_sub(size);
                    deleted_bytes += size;
                    deleted_entries += 1;
                }
            }
        }

        // Persist updated indices.
        for query in &mut query_states {
            if query.dirty {
                let _ = self.write_query_index(&query.dir, &query.index);
                query.dirty = false;
            }
        }

        let after = compute_stats_for_states(&query_states);
        Ok(DerivedCacheGcReport {
            before,
            after,
            deleted_bytes,
            deleted_entries,
        })
    }

    fn entry_path(
        &self,
        query_name: &str,
        query_schema_version: u32,
        args: &impl Serialize,
        input_fingerprints: &BTreeMap<String, Fingerprint>,
    ) -> Result<(PathBuf, Fingerprint), CacheError> {
        let safe_query = sanitize_component(query_name);
        let query_dir = self.root.join(safe_query);
        std::fs::create_dir_all(&query_dir)?;

        let fingerprint =
            Self::key_fingerprint(query_name, query_schema_version, args, input_fingerprints)?;
        let path = query_dir.join(format!("{}.bin", fingerprint.as_str()));
        Ok((path, fingerprint))
    }

    fn load_query_states(&self) -> Result<Vec<QueryState>, CacheError> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }

        let mut states = Vec::new();
        for entry in std::fs::read_dir(&self.root)? {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    tracing::debug!(
                        target = "nova.cache",
                        root = %self.root.display(),
                        error = %err,
                        "failed to read derived cache query directory entry"
                    );
                    continue;
                }
            };
            let path = entry.path();
            let meta = match std::fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                Err(err) => {
                    // Cache entries can race with deletion; only log unexpected errors.
                    if err.kind() != std::io::ErrorKind::NotFound {
                        tracing::debug!(
                            target = "nova.cache",
                            path = %path.display(),
                            error = %err,
                            "failed to stat derived cache query directory"
                        );
                    }
                    continue;
                }
            };
            let file_type = meta.file_type();
            if !file_type.is_dir() || file_type.is_symlink() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            let (index, dirty) = self.load_or_rebuild_query_index(&path)?;
            states.push(QueryState {
                name,
                dir: path,
                index,
                dirty,
            });
        }
        states.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(states)
    }

    fn load_or_rebuild_query_index(
        &self,
        query_dir: &Path,
    ) -> Result<(DerivedQueryIndex, bool), CacheError> {
        let index_path = query_dir.join(DERIVED_CACHE_INDEX_FILE_NAME);

        let mut dirty = false;
        let mut index = match std::fs::File::open(&index_path) {
            Ok(file) => {
                match serde_json::from_reader::<_, DerivedQueryIndex>(std::io::BufReader::new(file))
                {
                    Ok(mut index) => {
                        if index.schema_version != DERIVED_CACHE_INDEX_SCHEMA_VERSION
                            || !index.is_safe()
                        {
                            dirty = true;
                            index = DerivedQueryIndex::empty();
                        }
                        index
                    }
                    Err(err) => {
                        tracing::debug!(
                            target = "nova.cache",
                            path = %index_path.display(),
                            error = %err,
                            "failed to decode derived cache query index; rebuilding"
                        );
                        dirty = true;
                        DerivedQueryIndex::empty()
                    }
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                dirty = true;
                DerivedQueryIndex::empty()
            }
            Err(err) => {
                tracing::debug!(
                    target = "nova.cache",
                    path = %index_path.display(),
                    error = %err,
                    "failed to open derived cache query index; rebuilding"
                );
                dirty = true;
                DerivedQueryIndex::empty()
            }
        };

        // Reconcile index with actual directory contents so stray/untracked files
        // are still eligible for eviction.
        let mut observed = BTreeSet::new();
        for entry in std::fs::read_dir(query_dir)? {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    tracing::debug!(
                        target = "nova.cache",
                        query_dir = %query_dir.display(),
                        error = %err,
                        "failed to read derived cache query directory entry while rebuilding index"
                    );
                    continue;
                }
            };
            let file_name = entry.file_name().to_string_lossy().to_string();
            if file_name == DERIVED_CACHE_INDEX_FILE_NAME {
                continue;
            }
            // Ignore in-flight atomic writes. `atomic_write` uses unique temp file
            // names like `<dest>.tmp.<pid>.<counter>`.
            if file_name.ends_with(".tmp") || file_name.contains(".tmp.") {
                continue;
            }

            if !is_safe_entry_file_name(&file_name) {
                // Something weird in the directory (or lossy unicode conversion) - don't touch it.
                continue;
            }

            let path = entry.path();
            let meta = match std::fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                Err(err) => {
                    // Cache entries can race with deletion; only log unexpected errors.
                    if err.kind() != std::io::ErrorKind::NotFound {
                        tracing::debug!(
                            target = "nova.cache",
                            path = %path.display(),
                            error = %err,
                            "failed to stat derived cache entry while rebuilding index"
                        );
                    }
                    continue;
                }
            };
            let file_type = meta.file_type();
            if !file_type.is_file() && !file_type.is_symlink() {
                continue;
            }

            observed.insert(file_name.clone());
            let size = meta.len();

            match index.entries.get_mut(&file_name) {
                Some(existing) => {
                    if existing.size != size {
                        existing.size = size;
                        dirty = true;
                    }
                }
                None => {
                    let saved_at_millis = if file_type.is_file()
                        && path.extension().and_then(|e| e.to_str()) == Some("bin")
                    {
                        read_saved_at_millis(&path).unwrap_or(0)
                    } else {
                        0
                    };
                    index.entries.insert(
                        file_name,
                        DerivedQueryIndexEntry {
                            saved_at_millis,
                            size,
                        },
                    );
                    dirty = true;
                }
            }
        }

        // Drop entries whose files are gone.
        let missing: Vec<_> = index
            .entries
            .keys()
            .filter(|name| !observed.contains(*name))
            .cloned()
            .collect();
        if !missing.is_empty() {
            dirty = true;
            for name in missing {
                index.entries.remove(&name);
            }
        }

        Ok((index, dirty))
    }

    fn write_query_index(
        &self,
        query_dir: &Path,
        index: &DerivedQueryIndex,
    ) -> Result<(), CacheError> {
        let index_path = query_dir.join(DERIVED_CACHE_INDEX_FILE_NAME);
        atomic_write_with(&index_path, |file| {
            serde_json::to_writer(file, index)?;
            Ok(())
        })
    }

    fn update_index_after_store(
        &self,
        entry_path: &Path,
        saved_at_millis: u64,
        size: u64,
    ) -> Result<(), CacheError> {
        let Some(query_dir) = entry_path.parent() else {
            return Ok(());
        };
        let Some(file_name) = entry_path.file_name().and_then(|s| s.to_str()) else {
            return Ok(());
        };
        if !is_safe_entry_file_name(file_name) {
            return Ok(());
        }

        let (mut index, mut dirty) = self.load_or_rebuild_query_index(query_dir)?;
        let needs_update = match index.entries.get(file_name) {
            Some(existing) => existing.saved_at_millis != saved_at_millis || existing.size != size,
            None => true,
        };

        if needs_update {
            index.entries.insert(
                file_name.to_string(),
                DerivedQueryIndexEntry {
                    saved_at_millis,
                    size,
                },
            );
            dirty = true;
        }
        if dirty {
            let _ = self.write_query_index(query_dir, &index);
        }
        Ok(())
    }

    fn delete_entry_file(&self, query_dir: &Path, file_name: &str) -> Result<bool, CacheError> {
        if !is_safe_entry_file_name(file_name) {
            return Ok(false);
        }

        let entry_path = query_dir.join(file_name);
        if !entry_path.starts_with(&self.root) {
            return Ok(false);
        }

        match std::fs::remove_file(&entry_path) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(true),
            Err(err) => Err(err.into()),
        }
    }
}

#[derive(Debug, Serialize)]
struct PersistedDerivedValue<'a, T: Serialize> {
    schema_version: u32,
    query_schema_version: u32,
    nova_version: String,
    saved_at_millis: u64,
    query_name: String,
    key_fingerprint: Fingerprint,
    value: &'a T,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedDerivedValueOwned<T> {
    schema_version: u32,
    query_schema_version: u32,
    nova_version: String,
    saved_at_millis: u64,
    query_name: String,
    key_fingerprint: Fingerprint,
    value: T,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DerivedQueryIndexEntry {
    saved_at_millis: u64,
    size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DerivedQueryIndex {
    schema_version: u32,
    entries: BTreeMap<String, DerivedQueryIndexEntry>,
}

impl DerivedQueryIndex {
    fn empty() -> Self {
        Self {
            schema_version: DERIVED_CACHE_INDEX_SCHEMA_VERSION,
            entries: BTreeMap::new(),
        }
    }

    fn is_safe(&self) -> bool {
        if self.schema_version != DERIVED_CACHE_INDEX_SCHEMA_VERSION {
            return false;
        }
        self.entries
            .keys()
            .all(|name| is_safe_entry_file_name(name) && name != DERIVED_CACHE_INDEX_FILE_NAME)
    }

    fn total_bytes(&self) -> u64 {
        self.entries.values().map(|m| m.size).sum()
    }

    fn totals(&self) -> (u64, u64) {
        (self.total_bytes(), self.entries.len() as u64)
    }
}

#[derive(Debug)]
struct QueryState {
    name: String,
    dir: PathBuf,
    index: DerivedQueryIndex,
    dirty: bool,
}

fn compute_stats_for_states(states: &[QueryState]) -> DerivedCacheStats {
    let mut stats = DerivedCacheStats::default();
    for state in states {
        let (bytes, entries) = state.index.totals();
        stats.total_bytes += bytes;
        stats.total_entries += entries;
        stats.per_query.insert(
            state.name.clone(),
            DerivedCacheQueryStats { bytes, entries },
        );
    }
    stats
}

fn is_safe_entry_file_name(file_name: &str) -> bool {
    if file_name == DERIVED_CACHE_INDEX_FILE_NAME {
        return false;
    }

    let mut components = Path::new(file_name).components();
    matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()
}

fn read_saved_at_millis(path: &Path) -> Option<u64> {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(err) => {
            // Cache misses are expected; only log unexpected filesystem errors.
            if err.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!(
                    target = "nova.cache",
                    path = %path.display(),
                    error = %err,
                    "failed to stat derived cache entry"
                );
            }
            return None;
        }
    };
    if meta.len() > crate::BINCODE_PAYLOAD_LIMIT_BYTES as u64 {
        return None;
    }

    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!(
                    target = "nova.cache",
                    path = %path.display(),
                    error = %err,
                    "failed to open derived cache entry"
                );
            }
            return None;
        }
    };
    let mut reader = std::io::BufReader::new(file);
    let (schema_version, _query_schema_version, nova_version, saved_at_millis): (
        u32,
        u32,
        String,
        u64,
    ) = match bincode_options_limited().deserialize_from(&mut reader) {
        Ok(value) => value,
        Err(err) => {
            tracing::debug!(
                target = "nova.cache",
                path = %path.display(),
                error = %err,
                "failed to decode derived cache entry header"
            );
            return None;
        }
    };

    if schema_version != DERIVED_CACHE_SCHEMA_VERSION {
        return None;
    }
    if nova_version != nova_core::NOVA_VERSION {
        return None;
    }
    Some(saved_at_millis)
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
