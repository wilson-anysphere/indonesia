//! Two-tier query cache implementation used by Nova's future incremental engine.
//!
//! This is intentionally generic over *what* is cached: values are stored as raw
//! bytes behind an `Arc` so eviction is always safe for Salsa-style snapshots.
//! Eviction drops cache references, but any outstanding `Arc` keeps the value
//! alive.
//!
//! Note: `QueryCache` is intentionally **in-memory only**. Persistent query
//! results should be stored via [`PersistentQueryCache`], which uses
//! `nova-cache`'s versioned persistence primitives.

use nova_cache::{CacheDir, DerivedArtifactCache, Fingerprint};
use nova_memory::{EvictionRequest, EvictionResult, MemoryCategory, MemoryEvictor, MemoryManager};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};

/// Two-tier query cache with a hot LRU and warm clock (second-chance) policy.
#[derive(Debug)]
pub struct QueryCache {
    name: String,
    inner: Mutex<QueryCacheInner>,
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
        let cache = Arc::new(Self {
            name: "query_cache".to_string(),
            inner: Mutex::new(QueryCacheInner {
                hot: LruTier::default(),
                warm: ClockTier::default(),
            }),
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
        let mut inner = self.inner.lock().unwrap();

        if let Some(value) = inner.hot.get(key) {
            return Some(value);
        }

        if let Some(value) = inner.warm.get(key) {
            // Promote to hot (keeping a copy in warm for simplicity).
            inner.hot.insert(key.to_string(), value.clone());
            self.update_tracker_locked(&inner);
            return Some(value);
        }

        None
    }

    pub fn insert(&self, key: String, value: Arc<Vec<u8>>) {
        let mut inner = self.inner.lock().unwrap();
        inner.hot.insert(key, value);
        self.update_tracker_locked(&inner);
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

        inner.warm.evict_to(target_warm, pressure);
        inner.hot.evict_to(target_hot, &mut inner.warm, pressure);
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
        let mut inner = self.inner.lock().unwrap();
        let before = Self::total_bytes(&inner);

        if request.target_bytes == 0 {
            inner.hot.clear();
            inner.warm.clear();
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
            // pressure we drop them.
            match pressure {
                nova_memory::MemoryPressure::Low | nova_memory::MemoryPressure::Medium => {
                    warm.insert(key, value);
                }
                nova_memory::MemoryPressure::High | nova_memory::MemoryPressure::Critical => {}
            }
        }
    }

    fn clear(&mut self) {
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

    fn evict_to(&mut self, target_bytes: u64, pressure: nova_memory::MemoryPressure) {
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
        }
    }

    fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
        self.bytes = 0;
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
    memory: Arc<QueryCache>,
    derived: Option<DerivedArtifactCache>,
}

#[derive(Serialize)]
struct VersionedArgs<'a, T: Serialize> {
    schema_version: u32,
    args: &'a T,
}

impl PersistentQueryCache {
    pub fn new(manager: &MemoryManager, cache_dir: Option<&CacheDir>) -> Self {
        Self {
            memory: QueryCache::new(manager),
            derived: cache_dir.map(|dir| DerivedArtifactCache::new(dir.queries_dir())),
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
        if let Some(value) = self.memory.get(&cache_key) {
            return Some(value);
        }

        let derived = self.derived.as_ref()?;
        let key_args = VersionedArgs {
            schema_version: query_schema_version,
            args,
        };
        let bytes: Vec<u8> = derived
            .load(query_name, &key_args, input_fingerprints)
            .ok()??;
        let value = Arc::new(bytes);
        self.memory.insert(cache_key, value.clone());
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

        self.memory.insert(cache_key, value.clone());

        let Some(derived) = self.derived.as_ref() else {
            return;
        };
        let key_args = VersionedArgs {
            schema_version: query_schema_version,
            args,
        };
        let _ = derived.store(query_name, &key_args, input_fingerprints, &*value);
    }
}

fn cache_key<T: Serialize>(
    query_name: &str,
    query_schema_version: u32,
    args: &T,
    input_fingerprints: &BTreeMap<String, Fingerprint>,
) -> Option<String> {
    let key_args = VersionedArgs {
        schema_version: query_schema_version,
        args,
    };
    let fingerprint =
        DerivedArtifactCache::key_fingerprint(query_name, &key_args, input_fingerprints).ok()?;
    Some(fingerprint.as_str().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nova_cache::CacheConfig;
    use nova_memory::{MemoryBudget, MemoryEvictor};
    use serde::{Deserialize, Serialize};
    use tempfile::TempDir;

    #[derive(Debug, Serialize, Deserialize)]
    struct PersistedDerivedValueOwned<T> {
        schema_version: u32,
        nova_version: String,
        saved_at_millis: u64,
        query_name: String,
        key_fingerprint: Fingerprint,
        value: T,
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
        let _ = cache1.memory.evict(EvictionRequest {
            pressure: nova_memory::MemoryPressure::Critical,
            target_bytes: 0,
        });

        let query_dir = cache_dir.queries_dir().join("type_of");
        let entry_path = std::fs::read_dir(&query_dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();

        let bytes = std::fs::read(&entry_path).unwrap();
        let mut persisted: PersistedDerivedValueOwned<Vec<u8>> =
            bincode::deserialize(&bytes).unwrap();
        persisted.schema_version = persisted.schema_version.saturating_add(1);
        let mutated = bincode::serialize(&persisted).unwrap();
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

        let _ = cache1.memory.evict(EvictionRequest {
            pressure: nova_memory::MemoryPressure::Critical,
            target_bytes: 0,
        });

        let query_dir = cache_dir.queries_dir().join("type_of");
        let entry_path = std::fs::read_dir(&query_dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();

        let bytes = std::fs::read(&entry_path).unwrap();
        let mut persisted: PersistedDerivedValueOwned<Vec<u8>> =
            bincode::deserialize(&bytes).unwrap();
        persisted.nova_version = "0.0.0-test".to_string();
        let mutated = bincode::serialize(&persisted).unwrap();
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

        let _ = cache1.memory.evict(EvictionRequest {
            pressure: nova_memory::MemoryPressure::Critical,
            target_bytes: 0,
        });

        let query_dir = cache_dir.queries_dir().join("type_of");
        let entry_path = std::fs::read_dir(&query_dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();

        std::fs::write(&entry_path, b"not a valid bincode payload").unwrap();

        let cache2 = PersistentQueryCache::new(&make_manager(), Some(&cache_dir));
        assert!(cache2.get("type_of", 1, &args, &inputs).is_none());
    }
}
