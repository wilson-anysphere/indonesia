use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};

use nova_memory::{EvictionRequest, EvictionResult, MemoryCategory, MemoryEvictor, MemoryManager};
use nova_syntax::JavaParseResult;

use crate::FileId;

const DEFAULT_ENTRY_CAP: usize = 64;

#[derive(Debug, Clone)]
pub struct JavaParseCacheValue {
    pub text: Arc<String>,
    pub parse: Arc<JavaParseResult>,
}

#[derive(Debug)]
struct JavaParseCacheEntry {
    text: Arc<String>,
    parse: Arc<JavaParseResult>,
}

#[derive(Debug, Default)]
struct JavaParseCacheInner {
    map: HashMap<FileId, JavaParseCacheEntry>,
    /// LRU order: front = least-recent, back = most-recent.
    order: VecDeque<FileId>,
}

/// Side cache for incremental Java reparsing.
///
/// This cache is intentionally *not* part of Salsa memo tables; it is used as an optimization
/// when recomputing `parse_java` after a file content change.
///
/// ## Memory accounting
///
/// The cache stores `Arc<JavaParseResult>` values that are typically also memoized by Salsa.
/// To avoid double-counting shared allocations, this cache reports **0 bytes** to the memory
/// manager. The underlying parse allocations are accounted via Nova's Salsa memo footprint
/// tracking (`MemoryCategory::QueryCache`).
///
/// Memory safety / eviction:
/// - Values are stored behind `Arc` so outstanding Salsa snapshots remain valid.
/// - The cache is size-bounded (entry cap) to avoid unbounded retention.
/// - Callers should clear the cache on aggressive memo eviction to avoid defeating memory
///   reclamation.
#[derive(Debug)]
pub struct JavaParseCache {
    name: String,
    inner: Mutex<JavaParseCacheInner>,
    entry_cap: usize,
    registration: OnceLock<nova_memory::MemoryRegistration>,
    tracker: OnceLock<nova_memory::MemoryTracker>,
}

impl Default for JavaParseCache {
    fn default() -> Self {
        Self::new()
    }
}

impl JavaParseCache {
    pub fn new() -> Self {
        Self {
            name: "java_parse_cache".to_string(),
            inner: Mutex::new(JavaParseCacheInner::default()),
            entry_cap: DEFAULT_ENTRY_CAP,
            registration: OnceLock::new(),
            tracker: OnceLock::new(),
        }
    }

    pub fn register(self: &Arc<Self>, manager: &MemoryManager) {
        // `Database::register_salsa_memo_evictor` is expected to be called during workspace
        // initialization, but can be reached from multiple entrypoints. Use `OnceLock`'s
        // initialization primitives to avoid panicking if the cache is registered concurrently.
        let registration = self.registration.get_or_init(|| {
            manager.register_evictor(self.name.clone(), MemoryCategory::SyntaxTrees, self.clone())
        });
        let _ = self.tracker.get_or_init(|| registration.tracker());

        // Ensure the initial 0-byte accounting is visible immediately.
        let inner = self.lock_inner();
        self.update_tracker_locked(&inner);
    }

    fn lock_inner(&self) -> std::sync::MutexGuard<'_, JavaParseCacheInner> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                tracing::error!(
                    target = "nova.db",
                    name = %self.name,
                    "mutex poisoned; recovering java parse cache state"
                );
                poisoned.into_inner()
            }
        }
    }

    fn update_tracker_locked(&self, inner: &JavaParseCacheInner) {
        let Some(tracker) = self.tracker.get() else {
            return;
        };
        // Avoid double-counting with Salsa memo footprint tracking: the allocations cached here
        // are shared `Arc` values that Salsa typically memoizes as well.
        let _ = inner;
        tracker.set_bytes(0);
    }

    pub fn clear(&self) {
        let mut inner = self.lock_inner();
        inner.map.clear();
        inner.order.clear();
        self.update_tracker_locked(&inner);
    }

    pub fn get(&self, file: FileId) -> Option<JavaParseCacheValue> {
        let mut inner = self.lock_inner();
        let (text, parse) = {
            let entry = inner.map.get(&file)?;
            (entry.text.clone(), entry.parse.clone())
        };

        // Move to the back (most-recent).
        if let Some(pos) = inner.order.iter().position(|f| *f == file) {
            inner.order.remove(pos);
        }
        inner.order.push_back(file);

        Some(JavaParseCacheValue { text, parse })
    }

    pub fn insert(&self, file: FileId, text: Arc<String>, parse: Arc<JavaParseResult>) {
        let mut inner = self.lock_inner();

        if let Some(prev) = inner.map.insert(file, JavaParseCacheEntry { text, parse }) {
            let _ = prev;
        }

        if let Some(pos) = inner.order.iter().position(|f| *f == file) {
            inner.order.remove(pos);
        }
        inner.order.push_back(file);

        while inner.map.len() > self.entry_cap {
            let Some(oldest) = inner.order.pop_front() else {
                break;
            };
            let Some(evicted) = inner.map.remove(&oldest) else {
                continue;
            };
            let _ = evicted;
        }

        self.update_tracker_locked(&inner);
    }

    fn evict_to(&self, target_bytes: u64) {
        // This cache intentionally reports 0 bytes (see memory accounting docs), so eviction via
        // target bytes is a no-op. We keep the method so `MemoryEvictor::evict` can still call
        // `clear()` under critical pressure.
        let _ = target_bytes;

        let inner = self.lock_inner();
        self.update_tracker_locked(&inner);
    }

    #[cfg(test)]
    pub fn entry_count(&self) -> usize {
        self.lock_inner().map.len()
    }
}

impl MemoryEvictor for JavaParseCache {
    fn name(&self) -> &str {
        &self.name
    }

    fn category(&self) -> MemoryCategory {
        MemoryCategory::SyntaxTrees
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
