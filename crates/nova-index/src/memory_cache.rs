use nova_memory::{EvictionRequest, EvictionResult, MemoryCategory, MemoryEvictor, MemoryManager};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

/// A lightweight in-memory cache for cold index artifacts.
///
/// This is a building block for Nova's eventual multi-tier indexing story. The
/// cache is kept intentionally generic (values are raw bytes) so index builders
/// can store serialized representations and evict safely.
#[derive(Debug)]
pub struct IndexCache {
    name: String,
    inner: Mutex<IndexCacheInner>,
    registration: OnceLock<nova_memory::MemoryRegistration>,
    tracker: OnceLock<nova_memory::MemoryTracker>,
}

#[derive(Debug, Default)]
struct IndexCacheInner {
    map: HashMap<String, Arc<Vec<u8>>>,
    order: VecDeque<String>,
    bytes: u64,
}

impl IndexCache {
    pub fn new(manager: &MemoryManager) -> Arc<Self> {
        let cache = Arc::new(Self {
            name: "indexes".to_string(),
            inner: Mutex::new(IndexCacheInner::default()),
            registration: OnceLock::new(),
            tracker: OnceLock::new(),
        });

        let registration =
            manager.register_evictor(cache.name.clone(), MemoryCategory::Indexes, cache.clone());
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

    pub fn insert(&self, key: String, value: Arc<Vec<u8>>) {
        let mut inner = self.lock_inner();
        if let Some(prev) = inner.map.insert(key.clone(), value.clone()) {
            inner.bytes = inner.bytes.saturating_sub(prev.len() as u64);
        }
        inner.bytes = inner.bytes.saturating_add(value.len() as u64);
        if let Some(pos) = inner.order.iter().position(|k| k == &key) {
            inner.order.remove(pos);
        }
        inner.order.push_back(key);
        self.update_tracker_locked(&inner);
    }

    pub fn get(&self, key: &str) -> Option<Arc<Vec<u8>>> {
        let mut inner = self.lock_inner();
        let value = inner.map.get(key)?.clone();
        if let Some(pos) = inner.order.iter().position(|k| k == key) {
            inner.order.remove(pos);
        }
        inner.order.push_back(key.to_string());
        Some(value)
    }

    #[track_caller]
    fn lock_inner(&self) -> MutexGuard<'_, IndexCacheInner> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(err) => {
                let loc = std::panic::Location::caller();
                tracing::error!(
                    target = "nova.index",
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

    fn update_tracker_locked(&self, inner: &IndexCacheInner) {
        if let Some(tracker) = self.tracker.get() {
            tracker.set_bytes(inner.bytes);
        }
    }

    fn evict_to(&self, target: u64) {
        let mut inner = self.lock_inner();
        while inner.bytes > target {
            let Some(key) = inner.order.pop_front() else {
                break;
            };
            if let Some(value) = inner.map.remove(&key) {
                inner.bytes = inner.bytes.saturating_sub(value.len() as u64);
            }
        }
        self.update_tracker_locked(&inner);
    }

    fn clear(&self) {
        let mut inner = self.lock_inner();
        inner.map.clear();
        inner.order.clear();
        inner.bytes = 0;
        self.update_tracker_locked(&inner);
    }
}

impl MemoryEvictor for IndexCache {
    fn name(&self) -> &str {
        &self.name
    }

    fn category(&self) -> MemoryCategory {
        MemoryCategory::Indexes
    }

    fn evict(&self, request: EvictionRequest) -> EvictionResult {
        let before = self.tracker.get().map(|t| t.bytes()).unwrap_or(0);
        if request.target_bytes == 0
            || matches!(request.pressure, nova_memory::MemoryPressure::Critical)
        {
            self.clear();
        } else {
            self.evict_to(request.target_bytes);
        }
        let after = self.tracker.get().map(|t| t.bytes()).unwrap_or(0);
        EvictionResult {
            before_bytes: before,
            after_bytes: after,
        }
    }
}
