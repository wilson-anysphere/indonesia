//! Two-tier query cache implementation used by Nova's future incremental engine.
//!
//! This is intentionally generic over *what* is cached: values are stored as raw
//! bytes behind an `Arc` so eviction is always safe for Salsa-style snapshots.
//! Eviction drops cache references, but any outstanding `Arc` keeps the value
//! alive.
//!
//! `QueryCache` is primarily an in-memory cache. When constructed with a cache
//! directory (see [`QueryCache::new_with_disk`]) it will also persist cold values
//! to disk for warm starts via `nova-cache`'s [`QueryDiskCache`].
//!
//! The on-disk `QueryDiskCache` format is **best-effort** and intentionally *not*
//! a long-term stability guarantee: values are only reused when the cache schema
//! and Nova version match (and are assumed to be readable on the current
//! platform). Any mismatch, corruption, or I/O failure is treated as a cache
//! miss.
//!
//! For query-result persistence keyed by query name/arguments/input fingerprints,
//! use [`PersistentQueryCache`], which builds on `nova-cache`'s versioned
//! [`DerivedArtifactCache`].

use nova_cache::{CacheDir, DerivedArtifactCache, Fingerprint, QueryDiskCache};
use nova_memory::{EvictionRequest, EvictionResult, MemoryCategory, MemoryEvictor, MemoryManager};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

static DERIVED_CACHE_KEY_ERROR_LOGGED: OnceLock<()> = OnceLock::new();
static DERIVED_CACHE_LOAD_ERROR_LOGGED: OnceLock<()> = OnceLock::new();
static DERIVED_CACHE_STORE_ERROR_LOGGED: OnceLock<()> = OnceLock::new();

fn log_derived_cache_error_once<E: std::fmt::Display>(
    once: &'static OnceLock<()>,
    kind: &'static str,
    query_name: &str,
    query_schema_version: u32,
    err: &E,
) {
    if once.set(()).is_ok() {
        tracing::debug!(
            target = "nova.db",
            query_name,
            query_schema_version,
            error = %err,
            "{kind}"
        );
    }
}

/// Two-tier query cache with a hot LRU and warm clock (second-chance) policy.
#[derive(Debug)]
pub struct QueryCache {
    name: String,
    inner: Mutex<QueryCacheInner>,
    disk: Option<QueryDiskCache>,
    registration: OnceLock<nova_memory::MemoryRegistration>,
    tracker: OnceLock<nova_memory::MemoryTracker>,
}

#[derive(Debug)]
struct QueryCacheInner {
    hot: LruTier,
    warm: ClockTier,
}

#[derive(Debug, Default)]
struct LruTier {
    map: HashMap<String, Arc<Vec<u8>>>,
    order: VecDeque<String>,
    bytes: u64,
}

#[derive(Debug, Default)]
struct ClockTier {
    map: HashMap<String, ClockEntry>,
    order: VecDeque<String>,
    bytes: u64,
}

#[derive(Debug, Clone)]
struct ClockEntry {
    value: Arc<Vec<u8>>,
    referenced: bool,
}

impl QueryCache {
    pub fn new(manager: &MemoryManager) -> Arc<Self> {
        Self::new_with_disk(manager, None)
    }

    /// Create a query cache optionally backed by an on-disk [`QueryDiskCache`].
    ///
    /// `cache_dir` should be **project-scoped** (for example
    /// [`nova_cache::CacheDir::queries_dir`]). If a shared directory is used for
    /// multiple projects, identical keys will collide and can lead to incorrect
    /// cache reuse across projects.
    ///
    /// Disk persistence is best-effort: any I/O error, schema/version mismatch,
    /// or corruption is treated as a cache miss.
    pub fn new_with_disk(manager: &MemoryManager, cache_dir: Option<PathBuf>) -> Arc<Self> {
        // Use a dedicated subdirectory so our GC policy can't impact other
        // persistent query caches.
        let disk = cache_dir.and_then(|dir| QueryDiskCache::new(dir.join("query_cache")).ok());

        let cache = Arc::new(Self {
            name: "query_cache".to_string(),
            inner: Mutex::new(QueryCacheInner {
                hot: LruTier::default(),
                warm: ClockTier::default(),
            }),
            disk,
            registration: OnceLock::new(),
            tracker: OnceLock::new(),
        });

        let registration = manager.register_evictor(
            cache.name.clone(),
            MemoryCategory::QueryCache,
            cache.clone(),
        );
        cache
            .tracker
            .set(registration.tracker())
            .expect("tracker only set once");
        cache
            .registration
            .set(registration)
            .expect("registration only set once");

        cache
    }

    pub fn get(&self, key: &str) -> Option<Arc<Vec<u8>>> {
        let mut inner = self.lock_inner();

        if let Some(value) = inner.hot.get(key) {
            return Some(value);
        }

        if let Some(value) = inner.warm.get(key) {
            // Promote to hot (keeping a copy in warm for simplicity).
            inner.hot.insert(key.to_string(), value.clone());
            self.update_tracker_locked(&inner);
            return Some(value);
        }

        if let Some(store) = &self.disk {
            if let Ok(Some(bytes)) = store.load(key) {
                let value = Arc::new(bytes);
                inner.warm.insert(key.to_string(), value.clone());
                inner.hot.insert(key.to_string(), value.clone());
                self.update_tracker_locked(&inner);
                return Some(value);
            }
        }

        None
    }

    pub fn insert(&self, key: String, value: Arc<Vec<u8>>) {
        let mut inner = self.lock_inner();
        inner.hot.insert(key, value);
        self.update_tracker_locked(&inner);
    }

    #[track_caller]
    fn lock_inner(&self) -> MutexGuard<'_, QueryCacheInner> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(err) => {
                let loc = std::panic::Location::caller();
                tracing::error!(
                    target = "nova.db",
                    file = loc.file(),
                    line = loc.line(),
                    column = loc.column(),
                    error = %err,
                    "mutex poisoned; continuing with recovered guard"
                );
                err.into_inner()
            }
        }
    }

    fn total_bytes(inner: &QueryCacheInner) -> u64 {
        inner.hot.bytes + inner.warm.bytes
    }

    fn update_tracker_locked(&self, inner: &QueryCacheInner) {
        if let Some(tracker) = self.tracker.get() {
            tracker.set_bytes(Self::total_bytes(inner));
        }
    }

    fn shrink_locked(
        &self,
        inner: &mut QueryCacheInner,
        target_bytes: u64,
        pressure: nova_memory::MemoryPressure,
    ) {
        // Keep a small hot tier; evict from warm first.
        let target_hot = target_bytes / 5;
        let target_warm = target_bytes.saturating_sub(target_hot);

        inner.warm.evict_to(target_warm, &self.disk, pressure);
        inner
            .hot
            .evict_to(target_hot, &mut inner.warm, &self.disk, pressure);
    }
}

impl MemoryEvictor for QueryCache {
    fn name(&self) -> &str {
        &self.name
    }

    fn category(&self) -> MemoryCategory {
        MemoryCategory::QueryCache
    }

    fn evict(&self, request: EvictionRequest) -> EvictionResult {
        let mut inner = self.lock_inner();
        let before = Self::total_bytes(&inner);

        if request.target_bytes == 0 {
            inner.hot.clear(&self.disk);
            inner.warm.clear(&self.disk);
        } else {
            self.shrink_locked(&mut inner, request.target_bytes, request.pressure);
        }

        let after = Self::total_bytes(&inner);
        self.update_tracker_locked(&inner);
        EvictionResult {
            before_bytes: before,
            after_bytes: after,
        }
    }

    fn flush_to_disk(&self) -> std::io::Result<()> {
        let Some(store) = &self.disk else {
            return Ok(());
        };
        let inner = self.lock_inner();
        // Best-effort: persistent cache writes should never impact correctness.
        let _ = inner.warm.flush_all(store);
        Ok(())
    }
}

impl LruTier {
    fn get(&mut self, key: &str) -> Option<Arc<Vec<u8>>> {
        let value = self.map.get(key)?.clone();
        // Move to the back (most-recent).
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            self.order.remove(pos);
        }
        self.order.push_back(key.to_string());
        Some(value)
    }

    fn insert(&mut self, key: String, value: Arc<Vec<u8>>) {
        if let Some(prev) = self.map.insert(key.clone(), value.clone()) {
            self.bytes = self.bytes.saturating_sub(prev.len() as u64);
        }
        self.bytes = self.bytes.saturating_add(value.len() as u64);

        if let Some(pos) = self.order.iter().position(|k| k == &key) {
            self.order.remove(pos);
        }
        self.order.push_back(key);
    }

    fn evict_to(
        &mut self,
        target_bytes: u64,
        warm: &mut ClockTier,
        disk: &Option<QueryDiskCache>,
        pressure: nova_memory::MemoryPressure,
    ) {
        while self.bytes > target_bytes {
            let Some(key) = self.order.pop_front() else {
                break;
            };
            let Some(value) = self.map.remove(&key) else {
                continue;
            };
            self.bytes = self.bytes.saturating_sub(value.len() as u64);
            // Under low/medium pressure, keep demoted items in warm. Under high+
            // pressure we drop them (potentially after persisting).
            match pressure {
                nova_memory::MemoryPressure::Low | nova_memory::MemoryPressure::Medium => {
                    warm.insert(key, value);
                }
                nova_memory::MemoryPressure::High | nova_memory::MemoryPressure::Critical => {
                    if let Some(store) = disk {
                        let _ = store.store(&key, &value);
                    }
                }
            }
        }
    }

    fn clear(&mut self, disk: &Option<QueryDiskCache>) {
        if let Some(store) = disk {
            for (key, value) in &self.map {
                let _ = store.store(key, value);
            }
        }
        self.map.clear();
        self.order.clear();
        self.bytes = 0;
    }
}

impl ClockTier {
    fn get(&mut self, key: &str) -> Option<Arc<Vec<u8>>> {
        let entry = self.map.get_mut(key)?;
        entry.referenced = true;
        Some(entry.value.clone())
    }

    fn insert(&mut self, key: String, value: Arc<Vec<u8>>) {
        if let Some(prev) = self.map.insert(
            key.clone(),
            ClockEntry {
                value: value.clone(),
                referenced: true,
            },
        ) {
            self.bytes = self.bytes.saturating_sub(prev.value.len() as u64);
        }
        self.bytes = self.bytes.saturating_add(value.len() as u64);

        if let Some(pos) = self.order.iter().position(|k| k == &key) {
            self.order.remove(pos);
        }
        self.order.push_back(key);
    }

    fn evict_to(
        &mut self,
        target_bytes: u64,
        disk: &Option<QueryDiskCache>,
        pressure: nova_memory::MemoryPressure,
    ) {
        // Clock eviction: second chance via `referenced` bit.
        let mut spins = 0usize;
        while self.bytes > target_bytes && spins < self.order.len().saturating_mul(2).max(8) {
            spins += 1;
            let Some(key) = self.order.pop_front() else {
                break;
            };
            let Some(mut entry) = self.map.remove(&key) else {
                continue;
            };

            if entry.referenced
                && matches!(
                    pressure,
                    nova_memory::MemoryPressure::Low | nova_memory::MemoryPressure::Medium
                )
            {
                entry.referenced = false;
                self.map.insert(key.clone(), entry);
                self.order.push_back(key);
                continue;
            }

            self.bytes = self.bytes.saturating_sub(entry.value.len() as u64);
            if matches!(
                pressure,
                nova_memory::MemoryPressure::Low | nova_memory::MemoryPressure::Medium
            ) {
                if let Some(store) = disk {
                    let _ = store.store(&key, &entry.value);
                }
            }
        }
    }

    fn clear(&mut self, disk: &Option<QueryDiskCache>) {
        if let Some(store) = disk {
            for (key, entry) in &self.map {
                let _ = store.store(key, &entry.value);
            }
        }
        self.map.clear();
        self.order.clear();
        self.bytes = 0;
    }

    fn flush_all(&self, store: &QueryDiskCache) -> Result<(), nova_cache::CacheError> {
        for (key, entry) in &self.map {
            store.store(key, &entry.value)?;
        }
        Ok(())
    }
}

/// In-memory query cache backed by `nova-cache`'s versioned `DerivedArtifactCache`.
///
/// This combines [`QueryCache`] (for snapshot-safe in-memory caching) with
/// best-effort persistence that is safe across Nova versions and projects.
///
/// The on-disk format is not a stability guarantee: values are only reused when
/// the `nova-cache` derived-cache schema and Nova version match (and are assumed
/// to be read back on a compatible platform).
#[derive(Clone, Debug)]
pub struct PersistentQueryCache {
    memory: Option<Arc<QueryCache>>,
    derived: Option<DerivedArtifactCache>,
}

impl PersistentQueryCache {
    pub fn new(manager: &MemoryManager, cache_dir: Option<&CacheDir>) -> Self {
        Self {
            memory: Some(QueryCache::new(manager)),
            derived: cache_dir.map(|dir| DerivedArtifactCache::new(dir.queries_dir())),
        }
    }

    /// Create a cache that only persists to disk (no in-memory tier).
    ///
    /// This is convenient for Salsa queries, which already memoize results in
    /// memory but can benefit from a warm-start disk cache.
    pub fn new_derived(root: impl AsRef<Path>) -> Self {
        Self {
            memory: None,
            derived: Some(DerivedArtifactCache::new(root)),
        }
    }

    pub fn get<T: Serialize>(
        &self,
        query_name: &str,
        query_schema_version: u32,
        args: &T,
        input_fingerprints: &BTreeMap<String, Fingerprint>,
    ) -> Option<Arc<Vec<u8>>> {
        let cache_key = cache_key(query_name, query_schema_version, args, input_fingerprints)?;
        if let Some(memory) = &self.memory {
            if let Some(value) = memory.get(&cache_key) {
                return Some(value);
            }
        }

        let derived = self.derived.as_ref()?;
        let bytes: Vec<u8> =
            match derived.load(query_name, query_schema_version, args, input_fingerprints) {
                Ok(Some(bytes)) => bytes,
                Ok(None) => return None,
                Err(err) => {
                    log_derived_cache_error_once(
                        &DERIVED_CACHE_LOAD_ERROR_LOGGED,
                        "derived query cache load failed; treating as cache miss",
                        query_name,
                        query_schema_version,
                        &err,
                    );
                    return None;
                }
            };
        let value = Arc::new(bytes);
        if let Some(memory) = &self.memory {
            memory.insert(cache_key, value.clone());
        }
        Some(value)
    }

    pub fn insert<T: Serialize>(
        &self,
        query_name: &str,
        query_schema_version: u32,
        args: &T,
        input_fingerprints: &BTreeMap<String, Fingerprint>,
        value: Arc<Vec<u8>>,
    ) {
        let Some(cache_key) = cache_key(query_name, query_schema_version, args, input_fingerprints)
        else {
            return;
        };

        if let Some(memory) = &self.memory {
            memory.insert(cache_key, value.clone());
        }

        let Some(derived) = self.derived.as_ref() else {
            return;
        };
        if let Err(err) = derived.store(
            query_name,
            query_schema_version,
            args,
            input_fingerprints,
            &*value,
        ) {
            log_derived_cache_error_once(
                &DERIVED_CACHE_STORE_ERROR_LOGGED,
                "derived query cache store failed; ignoring",
                query_name,
                query_schema_version,
                &err,
            );
        }
    }

    /// Load a memoized value from disk or compute + persist it.
    ///
    /// Persistence is best-effort:
    /// - Load failures are treated as cache misses.
    /// - Store failures are ignored.
    pub fn get_or_compute<T, Args, F>(
        &self,
        query_name: &str,
        query_schema_version: u32,
        args: &Args,
        input_fingerprints: &BTreeMap<String, Fingerprint>,
        compute: F,
    ) -> T
    where
        T: Serialize + DeserializeOwned,
        Args: Serialize,
        F: FnOnce() -> T,
    {
        let Some(derived) = self.derived.as_ref() else {
            return compute();
        };

        match derived.load(query_name, query_schema_version, args, input_fingerprints) {
            Ok(Some(value)) => return value,
            Ok(None) => {}
            Err(err) => {
                log_derived_cache_error_once(
                    &DERIVED_CACHE_LOAD_ERROR_LOGGED,
                    "derived query cache load failed; treating as cache miss",
                    query_name,
                    query_schema_version,
                    &err,
                );
            }
        }

        let value = compute();
        if let Err(err) = derived.store(
            query_name,
            query_schema_version,
            args,
            input_fingerprints,
            &value,
        ) {
            log_derived_cache_error_once(
                &DERIVED_CACHE_STORE_ERROR_LOGGED,
                "derived query cache store failed; ignoring",
                query_name,
                query_schema_version,
                &err,
            );
        }
        value
    }
}

fn cache_key<T: Serialize>(
    query_name: &str,
    query_schema_version: u32,
    args: &T,
    input_fingerprints: &BTreeMap<String, Fingerprint>,
) -> Option<String> {
    let fingerprint = match DerivedArtifactCache::key_fingerprint(
        query_name,
        query_schema_version,
        args,
        input_fingerprints,
    ) {
        Ok(fingerprint) => fingerprint,
        Err(err) => {
            log_derived_cache_error_once(
                &DERIVED_CACHE_KEY_ERROR_LOGGED,
                "failed to compute derived query cache key; treating as cache miss",
                query_name,
                query_schema_version,
                &err,
            );
            return None;
        }
    };
    Some(fingerprint.as_str().to_string())
}

#[cfg(test)]
mod disk_cache_tests {
    use super::*;
    use bincode::Options;
    use nova_cache::QueryDiskCachePolicy;
    use nova_memory::MemoryBudget;
    use serde::{Deserialize, Serialize};
    use tempfile::TempDir;

    #[test]
    fn disk_cache_roundtrips() {
        let tmp = TempDir::new().unwrap();
        let cache = QueryDiskCache::new(tmp.path()).unwrap();

        cache.store("key", b"value").unwrap();
        assert_eq!(
            cache.load("key").unwrap().as_deref(),
            Some(b"value".as_slice())
        );
    }

    #[test]
    fn disk_cache_corruption_is_treated_as_miss() {
        let tmp = TempDir::new().unwrap();
        let cache = QueryDiskCache::new(tmp.path()).unwrap();

        cache.store("key", b"value").unwrap();

        let fingerprint = Fingerprint::from_bytes("key".as_bytes());
        let path = tmp.path().join(format!("{}.bin", fingerprint.as_str()));
        let bytes = std::fs::read(&path).unwrap();
        // Simulate a torn / partial write by truncating the payload.
        std::fs::write(&path, &bytes[..bytes.len() / 2]).unwrap();

        assert_eq!(cache.load("key").unwrap(), None);
    }

    #[test]
    fn disk_cache_detects_key_collisions() {
        let tmp = TempDir::new().unwrap();
        let cache = QueryDiskCache::new(tmp.path()).unwrap();

        cache.store("key1", b"value1").unwrap();

        // Simulate a fingerprint collision by forcing the *same file path*
        // (`fingerprint(key1)`) to contain a payload for a different key while keeping
        // `key_fingerprint` identical. This ensures the collision defense is
        // actually checking the stored full key, not just the fingerprint.
        #[derive(Debug, Serialize)]
        struct ForgedPersistedQueryValue {
            schema_version: u32,
            nova_version: String,
            saved_at_millis: u64,
            key: String,
            key_fingerprint: Fingerprint,
            value: Vec<u8>,
        }

        let fingerprint = Fingerprint::from_bytes("key1".as_bytes());
        let path = tmp.path().join(format!("{}.bin", fingerprint.as_str()));

        let forged = ForgedPersistedQueryValue {
            schema_version: nova_cache::QUERY_DISK_CACHE_SCHEMA_VERSION,
            nova_version: nova_core::NOVA_VERSION.to_string(),
            saved_at_millis: nova_cache::now_millis(),
            key: "key2".to_string(),
            key_fingerprint: fingerprint,
            value: b"value2".to_vec(),
        };
        let bytes = bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_little_endian()
            .serialize(&forged)
            .unwrap();
        std::fs::write(&path, bytes).unwrap();

        assert_eq!(cache.load("key1").unwrap(), None);
        assert!(
            path.exists(),
            "collision misses should not delete the underlying cache entry"
        );
    }

    #[test]
    fn disk_cache_schema_version_mismatch_is_treated_as_miss() {
        let tmp = TempDir::new().unwrap();
        let cache = QueryDiskCache::new(tmp.path()).unwrap();

        cache.store("key", b"value").unwrap();

        #[derive(Debug, Serialize, Deserialize)]
        struct PersistedQueryValueOwned {
            schema_version: u32,
            nova_version: String,
            saved_at_millis: u64,
            key: String,
            key_fingerprint: Fingerprint,
            value: Vec<u8>,
        }

        let fingerprint = Fingerprint::from_bytes("key".as_bytes());
        let path = tmp.path().join(format!("{}.bin", fingerprint.as_str()));
        let bytes = std::fs::read(&path).unwrap();
        let mut persisted: PersistedQueryValueOwned = bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_little_endian()
            .deserialize(&bytes)
            .unwrap();
        persisted.schema_version = persisted.schema_version.saturating_add(1);
        let bytes = bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_little_endian()
            .serialize(&persisted)
            .unwrap();
        std::fs::write(&path, bytes).unwrap();

        assert_eq!(cache.load("key").unwrap(), None);
    }

    #[test]
    fn disk_cache_nova_version_mismatch_is_treated_as_miss() {
        let tmp = TempDir::new().unwrap();
        let cache = QueryDiskCache::new(tmp.path()).unwrap();

        cache.store("key", b"value").unwrap();

        #[derive(Debug, Serialize, Deserialize)]
        struct PersistedQueryValueOwned {
            schema_version: u32,
            nova_version: String,
            saved_at_millis: u64,
            key: String,
            key_fingerprint: Fingerprint,
            value: Vec<u8>,
        }

        let fingerprint = Fingerprint::from_bytes("key".as_bytes());
        let path = tmp.path().join(format!("{}.bin", fingerprint.as_str()));
        let bytes = std::fs::read(&path).unwrap();
        let mut persisted: PersistedQueryValueOwned = bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_little_endian()
            .deserialize(&bytes)
            .unwrap();
        persisted.nova_version = "0.0.0-test".to_string();
        let bytes = bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_little_endian()
            .serialize(&persisted)
            .unwrap();
        std::fs::write(&path, bytes).unwrap();

        assert_eq!(cache.load("key").unwrap(), None);
    }

    #[test]
    fn disk_cache_garbage_data_is_treated_as_miss() {
        let tmp = TempDir::new().unwrap();
        let cache = QueryDiskCache::new(tmp.path()).unwrap();

        cache.store("key", b"value").unwrap();

        let fingerprint = Fingerprint::from_bytes("key".as_bytes());
        let path = tmp.path().join(format!("{}.bin", fingerprint.as_str()));
        std::fs::write(&path, b"not a valid bincode payload").unwrap();

        assert_eq!(cache.load("key").unwrap(), None);
    }

    #[test]
    fn disk_cache_oversized_entry_is_deleted_and_treated_as_miss() {
        let tmp = TempDir::new().unwrap();
        let cache = QueryDiskCache::new(tmp.path()).unwrap();

        let fingerprint = Fingerprint::from_bytes("key".as_bytes());
        let path = tmp.path().join(format!("{}.bin", fingerprint.as_str()));
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(nova_cache::BINCODE_PAYLOAD_LIMIT_BYTES as u64 + 1)
            .unwrap();
        drop(file);

        assert_eq!(cache.load("key").unwrap(), None);
        assert!(!path.exists());
    }

    #[test]
    fn disk_cache_skips_entries_larger_than_policy_max_bytes() {
        let tmp = TempDir::new().unwrap();
        let cache = QueryDiskCache::new_with_policy(
            tmp.path(),
            QueryDiskCachePolicy {
                ttl_millis: u64::MAX,
                max_bytes: 1,
                gc_interval_millis: u64::MAX,
            },
        )
        .unwrap();

        cache.store("key", b"value").unwrap();
        assert_eq!(cache.load("key").unwrap(), None);

        let fingerprint = Fingerprint::from_bytes("key".as_bytes());
        let path = tmp.path().join(format!("{}.bin", fingerprint.as_str()));
        assert!(!path.exists());
    }

    #[test]
    fn disk_cache_load_deletes_entries_larger_than_policy_max_bytes() {
        let tmp = TempDir::new().unwrap();
        let cache = QueryDiskCache::new_with_policy(
            tmp.path(),
            QueryDiskCachePolicy {
                ttl_millis: u64::MAX,
                max_bytes: 1,
                gc_interval_millis: u64::MAX,
            },
        )
        .unwrap();

        let fingerprint = Fingerprint::from_bytes("key".as_bytes());
        let path = tmp.path().join(format!("{}.bin", fingerprint.as_str()));
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(2).unwrap();
        drop(file);

        assert_eq!(cache.load("key").unwrap(), None);
        assert!(!path.exists());
    }

    #[test]
    fn query_cache_flush_to_disk_persists_warm_entries() {
        let tmp = TempDir::new().unwrap();
        let manager = MemoryManager::new(MemoryBudget::from_total(1024 * 1024));

        let cache1 = QueryCache::new_with_disk(&manager, Some(tmp.path().to_path_buf()));
        cache1.insert("k".to_string(), Arc::new(b"value".to_vec()));

        // Force the value into the warm tier.
        let _ = nova_memory::MemoryEvictor::evict(
            cache1.as_ref(),
            EvictionRequest {
                pressure: nova_memory::MemoryPressure::Low,
                target_bytes: 1,
            },
        );

        nova_memory::MemoryEvictor::flush_to_disk(cache1.as_ref()).unwrap();
        drop(cache1);

        let cache2 = QueryCache::new_with_disk(&manager, Some(tmp.path().to_path_buf()));
        let loaded = cache2.get("k").expect("load from disk");
        assert_eq!(&*loaded, b"value");
    }

    #[test]
    fn query_cache_eviction_under_high_pressure_persists_to_disk() {
        let tmp = TempDir::new().unwrap();
        let manager = MemoryManager::new(MemoryBudget::from_total(1024 * 1024));

        let cache1 = QueryCache::new_with_disk(&manager, Some(tmp.path().to_path_buf()));
        cache1.insert("k".to_string(), Arc::new(b"value".to_vec()));

        let _ = nova_memory::MemoryEvictor::evict(
            cache1.as_ref(),
            EvictionRequest {
                pressure: nova_memory::MemoryPressure::High,
                target_bytes: 1,
            },
        );
        drop(cache1);

        let cache2 = QueryCache::new_with_disk(&manager, Some(tmp.path().to_path_buf()));
        let loaded = cache2.get("k").expect("load from disk");
        assert_eq!(&*loaded, b"value");
    }

    #[test]
    fn disk_cache_gc_expires_entries_by_saved_at_millis() {
        let tmp = TempDir::new().unwrap();
        let cache = QueryDiskCache::new_with_policy(
            tmp.path(),
            QueryDiskCachePolicy {
                ttl_millis: 1_000,
                max_bytes: u64::MAX,
                gc_interval_millis: 0,
            },
        )
        .unwrap();

        #[derive(Debug, Serialize)]
        struct PersistedQueryValue<'a> {
            schema_version: u32,
            nova_version: String,
            saved_at_millis: u64,
            key: &'a str,
            key_fingerprint: Fingerprint,
            value: &'a [u8],
        }

        let saved_at_millis = nova_cache::now_millis().saturating_sub(10_000);
        let fingerprint = Fingerprint::from_bytes("key".as_bytes());
        let path = tmp.path().join(format!("{}.bin", fingerprint.as_str()));

        let persisted = PersistedQueryValue {
            schema_version: nova_cache::QUERY_DISK_CACHE_SCHEMA_VERSION,
            nova_version: nova_core::NOVA_VERSION.to_string(),
            saved_at_millis,
            key: "key",
            key_fingerprint: fingerprint,
            value: b"value",
        };
        let bytes = bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_little_endian()
            .serialize(&persisted)
            .unwrap();
        std::fs::write(&path, bytes).unwrap();

        cache.gc().unwrap();

        assert!(!path.exists(), "expired entry should be deleted by GC");
        assert_eq!(cache.load("key").unwrap(), None);
    }

    #[test]
    fn disk_cache_load_respects_ttl() {
        let tmp = TempDir::new().unwrap();
        let cache = QueryDiskCache::new_with_policy(
            tmp.path(),
            QueryDiskCachePolicy {
                ttl_millis: 1_000,
                max_bytes: u64::MAX,
                gc_interval_millis: u64::MAX,
            },
        )
        .unwrap();

        #[derive(Debug, Serialize)]
        struct PersistedQueryValue<'a> {
            schema_version: u32,
            nova_version: String,
            saved_at_millis: u64,
            key: &'a str,
            key_fingerprint: Fingerprint,
            value: &'a [u8],
        }

        let saved_at_millis = nova_cache::now_millis().saturating_sub(10_000);
        let fingerprint = Fingerprint::from_bytes("key".as_bytes());
        let path = tmp.path().join(format!("{}.bin", fingerprint.as_str()));

        let persisted = PersistedQueryValue {
            schema_version: nova_cache::QUERY_DISK_CACHE_SCHEMA_VERSION,
            nova_version: nova_core::NOVA_VERSION.to_string(),
            saved_at_millis,
            key: "key",
            key_fingerprint: fingerprint,
            value: b"value",
        };
        let bytes = bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_little_endian()
            .serialize(&persisted)
            .unwrap();
        std::fs::write(&path, bytes).unwrap();

        assert_eq!(cache.load("key").unwrap(), None);
        assert!(!path.exists());
    }

    #[test]
    fn disk_cache_gc_enforces_max_bytes_oldest_first() {
        let tmp = TempDir::new().unwrap();

        #[derive(Debug, Serialize)]
        struct PersistedQueryValue<'a> {
            schema_version: u32,
            nova_version: String,
            saved_at_millis: u64,
            key: &'a str,
            key_fingerprint: Fingerprint,
            value: &'a [u8],
        }

        let now = nova_cache::now_millis();

        let value_old = vec![0u8; 128];
        let value_new = vec![1u8; 128];

        let fp_old = Fingerprint::from_bytes("key1".as_bytes());
        let fp_new = Fingerprint::from_bytes("key2".as_bytes());

        let persisted_old = PersistedQueryValue {
            schema_version: nova_cache::QUERY_DISK_CACHE_SCHEMA_VERSION,
            nova_version: nova_core::NOVA_VERSION.to_string(),
            saved_at_millis: now.saturating_sub(10_000),
            key: "key1",
            key_fingerprint: fp_old.clone(),
            value: &value_old,
        };
        let persisted_new = PersistedQueryValue {
            schema_version: nova_cache::QUERY_DISK_CACHE_SCHEMA_VERSION,
            nova_version: nova_core::NOVA_VERSION.to_string(),
            saved_at_millis: now,
            key: "key2",
            key_fingerprint: fp_new.clone(),
            value: &value_new,
        };

        let opts = bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_little_endian();
        let bytes_old = opts.serialize(&persisted_old).unwrap();
        let bytes_new = opts.serialize(&persisted_new).unwrap();

        let cache = QueryDiskCache::new_with_policy(
            tmp.path(),
            QueryDiskCachePolicy {
                ttl_millis: u64::MAX,
                // Allow the newer entry to fit, but not both.
                max_bytes: bytes_new.len() as u64,
                gc_interval_millis: 0,
            },
        )
        .unwrap();

        let path_old = tmp.path().join(format!("{}.bin", fp_old.as_str()));
        let path_new = tmp.path().join(format!("{}.bin", fp_new.as_str()));
        std::fs::write(&path_old, bytes_old).unwrap();
        std::fs::write(&path_new, bytes_new).unwrap();

        cache.gc().unwrap();

        assert!(!path_old.exists(), "GC should evict the oldest entry first");
        assert!(path_new.exists(), "GC should keep the newest entry");
        assert_eq!(
            cache.load("key2").unwrap().as_deref(),
            Some(value_new.as_slice())
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bincode::Options;
    use nova_cache::CacheConfig;
    use nova_memory::{MemoryBudget, MemoryEvictor};
    use serde::{de::DeserializeOwned, Deserialize, Serialize};
    use tempfile::TempDir;

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

    fn bincode_options() -> impl bincode::Options {
        // Must match nova-cache's bincode settings (see `nova_cache::util::bincode_options`).
        bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_little_endian()
    }

    fn bincode_deserialize<T: DeserializeOwned>(bytes: &[u8]) -> T {
        bincode_options().deserialize(bytes).unwrap()
    }

    fn bincode_serialize<T: Serialize>(value: &T) -> Vec<u8> {
        bincode_options().serialize(value).unwrap()
    }

    fn make_cache_dir(tmp: &TempDir) -> CacheDir {
        let project_root = tmp.path().join("project");
        std::fs::create_dir_all(&project_root).unwrap();

        let cache_root = tmp.path().join("cache");
        std::fs::create_dir_all(&cache_root).unwrap();

        CacheDir::new(
            &project_root,
            CacheConfig {
                cache_root_override: Some(cache_root),
            },
        )
        .unwrap()
    }

    fn make_manager() -> MemoryManager {
        MemoryManager::new(MemoryBudget::from_total(10 * 1024 * 1024))
    }

    fn example_inputs() -> BTreeMap<String, Fingerprint> {
        let mut inputs = BTreeMap::new();
        inputs.insert("Main.java".to_string(), Fingerprint::from_bytes("v1"));
        inputs
    }

    fn persisted_entry_path<T: Serialize>(
        cache_dir: &CacheDir,
        query_name: &str,
        query_schema_version: u32,
        args: &T,
        inputs: &BTreeMap<String, Fingerprint>,
    ) -> PathBuf {
        let fingerprint =
            DerivedArtifactCache::key_fingerprint(query_name, query_schema_version, args, inputs)
                .unwrap();
        cache_dir
            .queries_dir()
            .join(query_name)
            .join(format!("{}.bin", fingerprint.as_str()))
    }

    #[test]
    fn persistent_query_cache_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let cache_dir = make_cache_dir(&tmp);

        let inputs = example_inputs();
        let args = ("Main.java".to_string(),);

        let cache1 = PersistentQueryCache::new(&make_manager(), Some(&cache_dir));
        cache1.insert(
            "type_of",
            1,
            &args,
            &inputs,
            Arc::new(b"answer:42".to_vec()),
        );
        drop(cache1);

        let cache2 = PersistentQueryCache::new(&make_manager(), Some(&cache_dir));
        let loaded = cache2.get("type_of", 1, &args, &inputs).unwrap();
        assert_eq!(&*loaded, b"answer:42");
    }

    #[test]
    fn persistent_query_cache_query_schema_version_mismatch_is_cache_miss() {
        let tmp = TempDir::new().unwrap();
        let cache_dir = make_cache_dir(&tmp);

        let inputs = example_inputs();
        let args = ("Main.java".to_string(),);

        let cache1 = PersistentQueryCache::new(&make_manager(), Some(&cache_dir));
        cache1.insert(
            "type_of",
            1,
            &args,
            &inputs,
            Arc::new(b"answer:42".to_vec()),
        );
        drop(cache1);

        let cache2 = PersistentQueryCache::new(&make_manager(), Some(&cache_dir));
        assert!(
            cache2.get("type_of", 2, &args, &inputs).is_none(),
            "query schema version is part of the persistence key"
        );
    }

    #[test]
    fn persistent_query_cache_cache_schema_version_mismatch_is_cache_miss() {
        let tmp = TempDir::new().unwrap();
        let cache_dir = make_cache_dir(&tmp);

        let inputs = example_inputs();
        let args = ("Main.java".to_string(),);

        let cache1 = PersistentQueryCache::new(&make_manager(), Some(&cache_dir));
        cache1.insert(
            "type_of",
            1,
            &args,
            &inputs,
            Arc::new(b"answer:42".to_vec()),
        );

        // Ensure we load from disk by clearing the in-memory cache.
        let _ = cache1
            .memory
            .as_ref()
            .expect("cache1 created with memory tier")
            .evict(EvictionRequest {
                pressure: nova_memory::MemoryPressure::Critical,
                target_bytes: 0,
            });

        let entry_path = persisted_entry_path(&cache_dir, "type_of", 1, &args, &inputs);

        let bytes = std::fs::read(&entry_path).unwrap();
        let mut persisted: PersistedDerivedValueOwned<Vec<u8>> = bincode_deserialize(&bytes);
        persisted.schema_version = persisted.schema_version.saturating_add(1);
        let mutated = bincode_serialize(&persisted);
        std::fs::write(&entry_path, mutated).unwrap();

        let cache2 = PersistentQueryCache::new(&make_manager(), Some(&cache_dir));
        assert!(cache2.get("type_of", 1, &args, &inputs).is_none());
    }

    #[test]
    fn persistent_query_cache_nova_version_mismatch_is_cache_miss() {
        let tmp = TempDir::new().unwrap();
        let cache_dir = make_cache_dir(&tmp);

        let inputs = example_inputs();
        let args = ("Main.java".to_string(),);

        let cache1 = PersistentQueryCache::new(&make_manager(), Some(&cache_dir));
        cache1.insert(
            "type_of",
            1,
            &args,
            &inputs,
            Arc::new(b"answer:42".to_vec()),
        );

        let _ = cache1
            .memory
            .as_ref()
            .expect("cache1 created with memory tier")
            .evict(EvictionRequest {
                pressure: nova_memory::MemoryPressure::Critical,
                target_bytes: 0,
            });

        let entry_path = persisted_entry_path(&cache_dir, "type_of", 1, &args, &inputs);

        let bytes = std::fs::read(&entry_path).unwrap();
        let mut persisted: PersistedDerivedValueOwned<Vec<u8>> = bincode_deserialize(&bytes);
        persisted.nova_version = "0.0.0-test".to_string();
        let mutated = bincode_serialize(&persisted);
        std::fs::write(&entry_path, mutated).unwrap();

        let cache2 = PersistentQueryCache::new(&make_manager(), Some(&cache_dir));
        assert!(cache2.get("type_of", 1, &args, &inputs).is_none());
    }

    #[test]
    fn persistent_query_cache_corruption_is_cache_miss() {
        let tmp = TempDir::new().unwrap();
        let cache_dir = make_cache_dir(&tmp);

        let inputs = example_inputs();
        let args = ("Main.java".to_string(),);

        let cache1 = PersistentQueryCache::new(&make_manager(), Some(&cache_dir));
        cache1.insert(
            "type_of",
            1,
            &args,
            &inputs,
            Arc::new(b"answer:42".to_vec()),
        );

        let _ = cache1
            .memory
            .as_ref()
            .expect("cache1 created with memory tier")
            .evict(EvictionRequest {
                pressure: nova_memory::MemoryPressure::Critical,
                target_bytes: 0,
            });

        let entry_path = persisted_entry_path(&cache_dir, "type_of", 1, &args, &inputs);

        std::fs::write(&entry_path, b"not a valid bincode payload").unwrap();

        let cache2 = PersistentQueryCache::new(&make_manager(), Some(&cache_dir));
        assert!(cache2.get("type_of", 1, &args, &inputs).is_none());
    }
}
