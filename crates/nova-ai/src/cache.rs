use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
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
struct CacheEntry {
    inserted_at: Instant,
    value: String,
}

#[derive(Debug, Default)]
struct CacheInner {
    map: HashMap<CacheKey, CacheEntry>,
    order: VecDeque<CacheKey>,
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
        let ttl = self.settings.ttl;
        let value = {
            let entry = inner.map.get(&key)?;
            if entry.inserted_at.elapsed() > ttl {
                None
            } else {
                Some(entry.value.clone())
            }
        };

        if let Some(value) = value {
            // Touch (move to most-recent).
            if let Some(pos) = inner.order.iter().position(|k| *k == key) {
                inner.order.remove(pos);
            }
            inner.order.push_back(key);
            return Some(value);
        }

        // Expired entry: remove it and return miss.
        inner.map.remove(&key);
        if let Some(pos) = inner.order.iter().position(|k| *k == key) {
            inner.order.remove(pos);
        }
        None
    }

    pub(crate) async fn insert(&self, key: CacheKey, value: String) {
        let mut inner = self.inner.lock().await;
        inner.map.insert(
            key,
            CacheEntry {
                inserted_at: Instant::now(),
                value,
            },
        );

        if let Some(pos) = inner.order.iter().position(|k| *k == key) {
            inner.order.remove(pos);
        }
        inner.order.push_back(key);

        self.prune_locked(&mut inner);
    }

    fn prune_locked(&self, inner: &mut CacheInner) {
        // Drop expired entries first.
        let ttl = self.settings.ttl;
        if ttl != Duration::ZERO {
            let now = Instant::now();
            let expired_keys: Vec<CacheKey> = inner
                .map
                .iter()
                .filter_map(|(key, entry)| {
                    (now.duration_since(entry.inserted_at) > ttl).then_some(*key)
                })
                .collect();

            for key in expired_keys {
                inner.map.remove(&key);
                if let Some(pos) = inner.order.iter().position(|k| *k == key) {
                    inner.order.remove(pos);
                }
            }
        }

        // Enforce max size via LRU order (front = least recent).
        while inner.map.len() > self.settings.max_entries {
            let Some(evicted_key) = inner.order.pop_front() else {
                break;
            };
            inner.map.remove(&evicted_key);
        }
    }
}

static CACHE_REGISTRY: OnceLock<Mutex<HashMap<CacheSettings, Weak<LlmResponseCache>>>> =
    OnceLock::new();

pub(crate) fn shared_cache(settings: CacheSettings) -> Arc<LlmResponseCache> {
    let registry = CACHE_REGISTRY.get_or_init(|| Mutex::new(HashMap::new()));
    let mut registry = crate::poison::lock(registry, "shared_cache.registry");

    if let Some(existing) = registry.get(&settings).and_then(|weak| weak.upgrade()) {
        return existing;
    }

    let cache = Arc::new(LlmResponseCache::new(settings));
    registry.insert(settings, Arc::downgrade(&cache));
    cache
}
