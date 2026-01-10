//! Two-tier query cache implementation used by Nova's future incremental engine.
//!
//! This is intentionally generic over *what* is cached: values are stored as raw
//! bytes behind an `Arc` so eviction is always safe for Salsa-style snapshots.
//! Eviction drops cache references, but any outstanding `Arc` keeps the value
//! alive.

use nova_memory::{EvictionRequest, EvictionResult, MemoryCategory, MemoryEvictor, MemoryManager};
use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

/// Two-tier query cache with a hot LRU and warm clock (second-chance) policy.
#[derive(Debug)]
pub struct QueryCache {
    name: String,
    inner: Mutex<QueryCacheInner>,
    disk: Option<DiskStore>,
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
    pub fn new(manager: &MemoryManager, cache_dir: Option<PathBuf>) -> Arc<Self> {
        let disk = cache_dir.map(DiskStore::new).transpose().ok().flatten();

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

        let registration =
            manager.register_evictor(cache.name.clone(), MemoryCategory::QueryCache, cache.clone());
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

        if let Some(store) = &self.disk {
            if let Ok(Some(bytes)) = store.get(key) {
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
        let mut inner = self.inner.lock().unwrap();
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
        let inner = self.inner.lock().unwrap();
        inner.warm.flush_all(store)?;
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
        disk: &Option<DiskStore>,
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
                        let _ = store.put(&key, &value);
                    }
                }
            }
        }
    }

    fn clear(&mut self, disk: &Option<DiskStore>) {
        if let Some(store) = disk {
            for (key, value) in &self.map {
                let _ = store.put(key, value);
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
        disk: &Option<DiskStore>,
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

            if entry.referenced {
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
                    let _ = store.put(&key, &entry.value);
                }
            }
        }
    }

    fn clear(&mut self, disk: &Option<DiskStore>) {
        if let Some(store) = disk {
            for (key, entry) in &self.map {
                let _ = store.put(key, &entry.value);
            }
        }
        self.map.clear();
        self.order.clear();
        self.bytes = 0;
    }

    fn flush_all(&self, store: &DiskStore) -> std::io::Result<()> {
        for (key, entry) in &self.map {
            store.put(key, &entry.value)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct DiskStore {
    dir: PathBuf,
}

impl DiskStore {
    fn new(dir: PathBuf) -> std::io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    fn put(&self, key: &str, value: &[u8]) -> std::io::Result<()> {
        std::fs::write(self.path_for(key), value)
    }

    fn get(&self, key: &str) -> std::io::Result<Option<Vec<u8>>> {
        let path = self.path_for(key);
        match std::fs::read(path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }

    fn path_for(&self, key: &str) -> PathBuf {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        key.hash(&mut hasher);
        let hash = hasher.finish();
        self.dir.join(format!("{hash:016x}.bin"))
    }
}

