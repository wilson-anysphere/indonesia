use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};
use tokio::sync::Mutex as TokioMutex;

/// Settings for the in-memory LLM response cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct CacheSettings {
    pub max_entries: usize,
    pub ttl: Duration,
}

/// A hash key used for LLM response caching.
///
/// This is intentionally opaque: we hash all identifying request parts (provider
/// + endpoint, model, parameters, sanitized prompt text, etc.) into a fixed-size
/// digest so we don't retain raw prompts in memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct CacheKey([u8; 32]);

impl CacheKey {
    pub(crate) fn from_hasher(hasher: Sha256) -> Self {
        let digest = hasher.finalize();
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&digest);
        Self(bytes)
    }
}

/// Incrementally build a [`CacheKey`] by hashing a sequence of typed fields.
pub(crate) struct CacheKeyBuilder {
    hasher: Sha256,
}

impl CacheKeyBuilder {
    pub(crate) fn new(namespace: &'static str) -> Self {
        let mut builder = Self {
            hasher: Sha256::new(),
        };
        builder.push_str(namespace);
        builder
    }

    pub(crate) fn push_str(&mut self, value: &str) {
        self.push_bytes(value.as_bytes());
    }

    pub(crate) fn push_bytes(&mut self, value: &[u8]) {
        let len: u64 = value
            .len()
            .try_into()
            .expect("prompt length should fit in u64");
        self.hasher.update(len.to_le_bytes());
        self.hasher.update(value);
    }

    pub(crate) fn push_u32(&mut self, value: u32) {
        self.hasher.update(value.to_le_bytes());
    }

    pub(crate) fn push_u64(&mut self, value: u64) {
        self.hasher.update(value.to_le_bytes());
    }

    pub(crate) fn finish(self) -> CacheKey {
        CacheKey::from_hasher(self.hasher)
    }
}

#[derive(Debug)]
struct CacheNode {
    key: CacheKey,
    inserted_at: Instant,
    value: String,
    lru_prev: Option<usize>,
    lru_next: Option<usize>,
    ttl_prev: Option<usize>,
    ttl_next: Option<usize>,
}

#[derive(Debug, Default)]
struct CacheInner {
    map: HashMap<CacheKey, usize>,
    nodes: Vec<Option<CacheNode>>,
    free_list: Vec<usize>,
    lru_head: Option<usize>,
    lru_tail: Option<usize>,
    ttl_head: Option<usize>,
    ttl_tail: Option<usize>,
}

/// An in-memory, tokio-friendly LRU+TTL cache for LLM completions.
///
/// Values are cached by [`CacheKey`] and store the completion string. Keys are
/// hashed so we avoid keeping raw prompts in memory.
#[derive(Debug)]
pub(crate) struct LlmResponseCache {
    settings: CacheSettings,
    inner: TokioMutex<CacheInner>,
}

impl LlmResponseCache {
    pub(crate) fn new(settings: CacheSettings) -> Self {
        Self {
            settings: CacheSettings {
                max_entries: settings.max_entries.max(1),
                ttl: settings.ttl,
            },
            inner: TokioMutex::new(CacheInner::default()),
        }
    }

    pub(crate) async fn get(&self, key: CacheKey) -> Option<String> {
        let mut inner = self.inner.lock().await;
        let &idx = inner.map.get(&key)?;
        let Some(node) = inner.nodes.get(idx).and_then(Option::as_ref) else {
            // Corrupted index; drop the map entry so the cache can recover.
            inner.map.remove(&key);
            return None;
        };

        if node.inserted_at.elapsed() > self.settings.ttl {
            Self::remove_locked(&mut inner, idx);
            return None;
        }

        let value = node.value.clone();
        Self::touch_lru_locked(&mut inner, idx);
        Some(value)
    }

    pub(crate) async fn insert(&self, key: CacheKey, value: String) {
        let mut inner = self.inner.lock().await;
        let now = Instant::now();

        if let Some(&idx) = inner.map.get(&key) {
            if let Some(node) = inner.nodes.get_mut(idx).and_then(Option::as_mut) {
                node.inserted_at = now;
                node.value = value;
                Self::touch_lru_locked(&mut inner, idx);
                Self::touch_ttl_locked(&mut inner, idx);
                self.prune_locked(&mut inner);
                return;
            }

            // Corrupted index; fall back to reinsertion.
            inner.map.remove(&key);
        }

        let idx = if let Some(idx) = inner.free_list.pop() {
            inner.nodes[idx] = Some(CacheNode {
                key,
                inserted_at: now,
                value,
                lru_prev: None,
                lru_next: None,
                ttl_prev: None,
                ttl_next: None,
            });
            idx
        } else {
            inner.nodes.push(Some(CacheNode {
                key,
                inserted_at: now,
                value,
                lru_prev: None,
                lru_next: None,
                ttl_prev: None,
                ttl_next: None,
            }));
            inner.nodes.len() - 1
        };

        inner.map.insert(key, idx);
        Self::push_lru_back_locked(&mut inner, idx);
        Self::push_ttl_back_locked(&mut inner, idx);
        self.prune_locked(&mut inner);
    }

    fn prune_locked(&self, inner: &mut CacheInner) {
        let ttl = self.settings.ttl;
        if ttl != Duration::ZERO {
            // TTL order (head = oldest insertion) lets us prune expired entries incrementally without
            // scanning the whole cache.
            let now = Instant::now();
            while let Some(idx) = inner.ttl_head {
                let Some(node) = inner.nodes.get(idx).and_then(Option::as_ref) else {
                    // Corrupted node; drop it from the list and keep going.
                    Self::remove_locked(inner, idx);
                    continue;
                };

                if now.duration_since(node.inserted_at) > ttl {
                    Self::remove_locked(inner, idx);
                } else {
                    break;
                }
            }
        }

        // Enforce max size via LRU order (front = least recent).
        while inner.map.len() > self.settings.max_entries {
            let Some(idx) = inner.lru_head else {
                break;
            };
            Self::remove_locked(inner, idx);
        }
    }

    fn touch_lru_locked(inner: &mut CacheInner, idx: usize) {
        if inner.lru_tail == Some(idx) {
            return;
        }
        Self::detach_lru_locked(inner, idx);
        Self::push_lru_back_locked(inner, idx);
    }

    fn touch_ttl_locked(inner: &mut CacheInner, idx: usize) {
        if inner.ttl_tail == Some(idx) {
            return;
        }
        Self::detach_ttl_locked(inner, idx);
        Self::push_ttl_back_locked(inner, idx);
    }

    fn push_lru_back_locked(inner: &mut CacheInner, idx: usize) {
        let tail = inner.lru_tail;
        {
            let node = inner.nodes[idx].as_mut().expect("node should exist");
            node.lru_prev = tail;
            node.lru_next = None;
        }

        if let Some(tail_idx) = tail {
            inner.nodes[tail_idx]
                .as_mut()
                .expect("tail should exist")
                .lru_next = Some(idx);
        } else {
            inner.lru_head = Some(idx);
        }
        inner.lru_tail = Some(idx);
    }

    fn push_ttl_back_locked(inner: &mut CacheInner, idx: usize) {
        let tail = inner.ttl_tail;
        {
            let node = inner.nodes[idx].as_mut().expect("node should exist");
            node.ttl_prev = tail;
            node.ttl_next = None;
        }

        if let Some(tail_idx) = tail {
            inner.nodes[tail_idx]
                .as_mut()
                .expect("tail should exist")
                .ttl_next = Some(idx);
        } else {
            inner.ttl_head = Some(idx);
        }
        inner.ttl_tail = Some(idx);
    }

    fn detach_lru_locked(inner: &mut CacheInner, idx: usize) {
        let (prev, next) = {
            let node = inner.nodes[idx].as_ref().expect("node should exist");
            (node.lru_prev, node.lru_next)
        };

        if let Some(prev_idx) = prev {
            inner.nodes[prev_idx]
                .as_mut()
                .expect("prev should exist")
                .lru_next = next;
        } else {
            inner.lru_head = next;
        }

        if let Some(next_idx) = next {
            inner.nodes[next_idx]
                .as_mut()
                .expect("next should exist")
                .lru_prev = prev;
        } else {
            inner.lru_tail = prev;
        }

        let node = inner.nodes[idx].as_mut().expect("node should exist");
        node.lru_prev = None;
        node.lru_next = None;
    }

    fn detach_ttl_locked(inner: &mut CacheInner, idx: usize) {
        let (prev, next) = {
            let node = inner.nodes[idx].as_ref().expect("node should exist");
            (node.ttl_prev, node.ttl_next)
        };

        if let Some(prev_idx) = prev {
            inner.nodes[prev_idx]
                .as_mut()
                .expect("prev should exist")
                .ttl_next = next;
        } else {
            inner.ttl_head = next;
        }

        if let Some(next_idx) = next {
            inner.nodes[next_idx]
                .as_mut()
                .expect("next should exist")
                .ttl_prev = prev;
        } else {
            inner.ttl_tail = prev;
        }

        let node = inner.nodes[idx].as_mut().expect("node should exist");
        node.ttl_prev = None;
        node.ttl_next = None;
    }

    fn remove_locked(inner: &mut CacheInner, idx: usize) {
        let is_valid = idx < inner.nodes.len() && inner.nodes[idx].is_some();
        if !is_valid {
            // Cache corruption: reset to an empty state so callers don't spin.
            *inner = CacheInner::default();
            return;
        }

        Self::detach_lru_locked(inner, idx);
        Self::detach_ttl_locked(inner, idx);

        let node = inner.nodes[idx].take().expect("node should exist");
        inner.map.remove(&node.key);
        inner.free_list.push(idx);
    }
}

static CACHE_REGISTRY: OnceLock<Mutex<HashMap<CacheSettings, Weak<LlmResponseCache>>>> =
    OnceLock::new();

pub(crate) fn shared_cache(settings: CacheSettings) -> Arc<LlmResponseCache> {
    let registry = CACHE_REGISTRY.get_or_init(|| Mutex::new(HashMap::new()));
    let mut registry = registry.lock().expect("cache registry mutex poisoned");

    if let Some(existing) = registry.get(&settings).and_then(|weak| weak.upgrade()) {
        return existing;
    }

    let cache = Arc::new(LlmResponseCache::new(settings));
    registry.insert(settings, Arc::downgrade(&cache));
    cache
}
