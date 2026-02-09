use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::mem;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};

fn warn_poisoned_embedding_cache_mutex_once() {
    static WARNED: OnceLock<()> = OnceLock::new();
    WARNED.get_or_init(|| {
        tracing::warn!(
            target = "nova.ai",
            "embedding cache mutex was poisoned by a previous panic; attempting best-effort recovery"
        );
    });
}

fn warn_poisoned_embedding_cache_registry_mutex_once() {
    static WARNED: OnceLock<()> = OnceLock::new();
    WARNED.get_or_init(|| {
        tracing::warn!(
            target = "nova.ai",
            "embedding cache registry mutex was poisoned by a previous panic; attempting best-effort recovery"
        );
    });
}

/// Opaque cache key for embedding vectors.
///
/// This is intentionally a fixed-size digest so we never store raw input text in memory as a cache
/// key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EmbeddingCacheKey([u8; 32]);

impl EmbeddingCacheKey {
    /// Create a new cache key by hashing `(embedder identity, model name, input text)`.
    pub fn new(embedder_identity: &str, model_name: &str, input_text: &str) -> Self {
        let mut builder = EmbeddingCacheKeyBuilder::new("nova_ai_embeddings_v1");
        builder.push_str(embedder_identity);
        builder.push_str(model_name);
        builder.push_str(input_text);
        builder.finish()
    }

    fn from_hasher(hasher: Sha256) -> Self {
        let digest = hasher.finalize();
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&digest);
        Self(bytes)
    }
}

/// Incrementally build an [`EmbeddingCacheKey`] by hashing a sequence of typed fields.
pub struct EmbeddingCacheKeyBuilder {
    hasher: Sha256,
}

impl EmbeddingCacheKeyBuilder {
    pub fn new(namespace: &'static str) -> Self {
        let mut builder = Self {
            hasher: Sha256::new(),
        };
        builder.push_str(namespace);
        builder
    }

    pub fn push_str(&mut self, value: &str) {
        self.push_bytes(value.as_bytes());
    }

    pub fn push_bytes(&mut self, value: &[u8]) {
        let len: u64 = value
            .len()
            .try_into()
            .expect("embedding text length should fit in u64");
        self.hasher.update(len.to_le_bytes());
        self.hasher.update(value);
    }

    pub fn finish(self) -> EmbeddingCacheKey {
        EmbeddingCacheKey::from_hasher(self.hasher)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EmbeddingCacheSettings {
    pub max_memory_bytes: usize,
}

#[derive(Debug)]
struct CacheNode {
    key: EmbeddingCacheKey,
    value: Vec<f32>,
    bytes: usize,
    prev: Option<usize>,
    next: Option<usize>,
}

#[derive(Debug, Default)]
struct CacheInner {
    map: HashMap<EmbeddingCacheKey, usize>,
    nodes: Vec<Option<CacheNode>>,
    free_list: Vec<usize>,
    head: Option<usize>,
    tail: Option<usize>,
    used_bytes: usize,
}

/// A thread-safe, memory-bounded LRU cache for embedding vectors.
///
/// The cache tracks an approximate memory usage per stored vector and evicts least-recently-used
/// entries until the cache is under its configured budget.
#[derive(Debug)]
pub struct EmbeddingVectorCache {
    max_memory_bytes: usize,
    inner: Mutex<CacheInner>,
}

impl EmbeddingVectorCache {
    /// Approximate per-entry overhead (in bytes) beyond the raw `Vec<f32>` allocation.
    ///
    /// The goal is to avoid unbounded cache growth even though the cache doesn't account for every
    /// byte of allocator / hash table overhead precisely.
    const ENTRY_OVERHEAD_BYTES: usize = 96;

    pub fn new(max_memory_bytes: usize) -> Self {
        Self {
            max_memory_bytes: max_memory_bytes.max(1),
            inner: Mutex::new(CacheInner::default()),
        }
    }

    /// Estimate the memory usage (in bytes) for an embedding vector with `dims` dimensions.
    pub fn estimate_entry_bytes(dims: usize) -> usize {
        dims.saturating_mul(mem::size_of::<f32>())
            .saturating_add(Self::ENTRY_OVERHEAD_BYTES)
    }

    fn estimate_vec_bytes(vec: &[f32]) -> usize {
        Self::estimate_entry_bytes(vec.len())
    }

    fn lock_inner(&self) -> MutexGuard<'_, CacheInner> {
        let mut guard = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        if self.inner.is_poisoned() {
            warn_poisoned_embedding_cache_mutex_once();
            *guard = CacheInner::default();
            self.inner.clear_poison();
        }
        guard
    }

    /// Look up an embedding vector and update its LRU position on a hit.
    pub fn get(&self, key: EmbeddingCacheKey) -> Option<Vec<f32>> {
        let mut inner = self.lock_inner();
        let &idx = inner.map.get(&key)?;
        let value = inner.nodes.get(idx)?.as_ref()?.value.clone();
        Self::touch_locked(&mut inner, idx);
        Some(value)
    }

    /// Insert an embedding vector into the cache.
    ///
    /// If the insertion pushes the cache over budget, least-recently-used entries are evicted
    /// until the cache is under budget again.
    pub fn insert(&self, key: EmbeddingCacheKey, value: Vec<f32>) {
        let bytes = Self::estimate_vec_bytes(&value);
        if bytes > self.max_memory_bytes {
            // The value can never fit in the cache; avoid an eviction loop.
            return;
        }

        let mut inner = self.lock_inner();

        if let Some(&idx) = inner.map.get(&key) {
            // Replace in-place.
            if inner.nodes.get(idx).and_then(Option::as_ref).is_some() {
                let old_bytes = {
                    let node = inner
                        .nodes
                        .get_mut(idx)
                        .and_then(Option::as_mut)
                        .expect("node should exist");
                    let old_bytes = node.bytes;
                    node.value = value;
                    node.bytes = bytes;
                    old_bytes
                };
                inner.used_bytes = inner.used_bytes.saturating_sub(old_bytes);
                inner.used_bytes = inner.used_bytes.saturating_add(bytes);
                Self::touch_locked(&mut inner, idx);
                Self::evict_to_budget_locked(&mut inner, self.max_memory_bytes);
                return;
            }

            // Corrupted index; fall back to reinsertion.
            inner.map.remove(&key);
        }

        let idx = if let Some(idx) = inner.free_list.pop() {
            inner.nodes[idx] = Some(CacheNode {
                key,
                value,
                bytes,
                prev: None,
                next: None,
            });
            idx
        } else {
            inner.nodes.push(Some(CacheNode {
                key,
                value,
                bytes,
                prev: None,
                next: None,
            }));
            inner.nodes.len() - 1
        };

        inner.map.insert(key, idx);
        inner.used_bytes = inner.used_bytes.saturating_add(bytes);
        Self::push_back_locked(&mut inner, idx);
        Self::evict_to_budget_locked(&mut inner, self.max_memory_bytes);
    }

    /// Current approximate memory usage for the cache.
    pub fn used_memory_bytes(&self) -> usize {
        self.lock_inner().used_bytes
    }

    /// Number of cached entries.
    pub fn len(&self) -> usize {
        self.lock_inner().map.len()
    }

    fn touch_locked(inner: &mut CacheInner, idx: usize) {
        if inner.tail == Some(idx) {
            return;
        }
        Self::detach_locked(inner, idx);
        Self::push_back_locked(inner, idx);
    }

    fn push_back_locked(inner: &mut CacheInner, idx: usize) {
        let tail = inner.tail;
        {
            let node = inner.nodes[idx].as_mut().expect("node should exist");
            node.prev = tail;
            node.next = None;
        }

        if let Some(tail_idx) = tail {
            inner.nodes[tail_idx]
                .as_mut()
                .expect("tail should exist")
                .next = Some(idx);
        } else {
            inner.head = Some(idx);
        }
        inner.tail = Some(idx);
    }

    fn detach_locked(inner: &mut CacheInner, idx: usize) {
        let (prev, next) = {
            let node = inner.nodes[idx].as_ref().expect("node should exist");
            (node.prev, node.next)
        };

        if let Some(prev_idx) = prev {
            inner.nodes[prev_idx]
                .as_mut()
                .expect("prev should exist")
                .next = next;
        } else {
            inner.head = next;
        }

        if let Some(next_idx) = next {
            inner.nodes[next_idx]
                .as_mut()
                .expect("next should exist")
                .prev = prev;
        } else {
            inner.tail = prev;
        }

        let node = inner.nodes[idx].as_mut().expect("node should exist");
        node.prev = None;
        node.next = None;
    }

    fn pop_front_locked(inner: &mut CacheInner) -> Option<usize> {
        let idx = inner.head?;
        let next = inner.nodes[idx].as_ref()?.next;
        inner.head = next;
        if let Some(next_idx) = next {
            inner.nodes[next_idx].as_mut()?.prev = None;
        } else {
            inner.tail = None;
        }

        let node = inner.nodes[idx].as_mut()?;
        node.prev = None;
        node.next = None;
        Some(idx)
    }

    fn remove_locked(inner: &mut CacheInner, idx: usize) {
        let Some(node) = inner.nodes[idx].take() else {
            return;
        };

        inner.used_bytes = inner.used_bytes.saturating_sub(node.bytes);
        inner.map.remove(&node.key);
        inner.free_list.push(idx);
    }

    fn evict_to_budget_locked(inner: &mut CacheInner, max_memory_bytes: usize) {
        while inner.used_bytes > max_memory_bytes {
            let Some(idx) = Self::pop_front_locked(inner) else {
                break;
            };
            Self::remove_locked(inner, idx);
        }
    }
}

static CACHE_REGISTRY: OnceLock<Mutex<HashMap<EmbeddingCacheSettings, Weak<EmbeddingVectorCache>>>> =
    OnceLock::new();

/// Create or reuse a shared embedding cache for the given settings.
pub fn shared_embedding_cache(settings: EmbeddingCacheSettings) -> Arc<EmbeddingVectorCache> {
    let registry = CACHE_REGISTRY.get_or_init(|| Mutex::new(HashMap::new()));
    let mut registry_guard = registry.lock().unwrap_or_else(|err| err.into_inner());
    if registry.is_poisoned() {
        warn_poisoned_embedding_cache_registry_mutex_once();
        registry_guard.clear();
        registry.clear_poison();
    }

    if let Some(existing) = registry_guard.get(&settings).and_then(|weak| weak.upgrade()) {
        return existing;
    }

    let cache = Arc::new(EmbeddingVectorCache::new(settings.max_memory_bytes));
    registry_guard.insert(settings, Arc::downgrade(&cache));
    cache
}
