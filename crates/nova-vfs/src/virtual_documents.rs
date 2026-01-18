use std::sync::{Arc, Mutex, MutexGuard};

use lru::LruCache;

use crate::path::VfsPath;

/// Thread-safe bounded store for synthesized virtual documents (e.g. decompiled sources).
///
/// The store is intentionally scoped to only the `nova:///decompiled/...` and legacy
/// `nova-decompile:///...` URI forms represented by:
/// - [`VfsPath::Decompiled`]
/// - [`VfsPath::LegacyDecompiled`]
///
/// All other [`VfsPath`] variants are ignored.
///
/// The store is bounded by a byte budget (`max_bytes`), counting bytes using `text.len()`.
/// When inserting would exceed the budget, least-recently-used entries are evicted until the
/// invariant is restored.
#[derive(Debug, Clone)]
pub struct VirtualDocumentStore {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Debug)]
struct Inner {
    max_bytes: usize,
    total_bytes: usize,
    lru: LruCache<VfsPath, Arc<str>>,
}

impl VirtualDocumentStore {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                max_bytes,
                total_bytes: 0,
                lru: LruCache::unbounded(),
            })),
        }
    }

    #[track_caller]
    fn lock_inner(&self) -> MutexGuard<'_, Inner> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(err) => {
                let loc = std::panic::Location::caller();
                tracing::error!(
                    target = "nova.vfs",
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

    /// Inserts a UTF-8 document into the store.
    ///
    /// Non-decompiled paths are ignored.
    pub fn insert_text(&self, path: VfsPath, text: String) {
        if !is_decompiled_path(&path) {
            return;
        }

        let text: Arc<str> = Arc::from(text);
        let bytes = text.len();

        let mut inner = self.lock_inner();

        // A budget of 0 means "store nothing". Also avoid inserting documents that can never fit
        // into the configured budget.
        if inner.max_bytes == 0 || bytes > inner.max_bytes {
            if let Some(prev) = inner.lru.pop(&path) {
                inner.total_bytes = inner.total_bytes.saturating_sub(prev.len());
            }
            return;
        }

        if let Some(prev) = inner.lru.put(path, text) {
            inner.total_bytes = inner.total_bytes.saturating_sub(prev.len());
        }
        inner.total_bytes = inner.total_bytes.saturating_add(bytes);

        while inner.total_bytes > inner.max_bytes {
            let Some((_evicted_path, evicted_text)) = inner.lru.pop_lru() else {
                inner.total_bytes = 0;
                break;
            };
            inner.total_bytes = inner.total_bytes.saturating_sub(evicted_text.len());
        }
    }

    /// Retrieves a UTF-8 document from the store.
    ///
    /// Non-decompiled paths always return `None`.
    pub fn get_text(&self, path: &VfsPath) -> Option<Arc<str>> {
        if !is_decompiled_path(path) {
            return None;
        }

        let mut inner = self.lock_inner();
        inner.lru.get(path).cloned()
    }

    /// Best-effort estimate of the total number of UTF-8 bytes stored in the cache.
    ///
    /// This tracks `text.len()` for each cached virtual document (not capacity) and is intended
    /// for coarse memory accounting and telemetry.
    pub fn estimated_bytes(&self) -> usize {
        let inner = self.lock_inner();
        inner.total_bytes
    }

    /// Returns whether the store contains an entry for `path`.
    ///
    /// Non-decompiled paths always return `false`.
    pub fn contains(&self, path: &VfsPath) -> bool {
        if !is_decompiled_path(path) {
            return false;
        }

        let inner = self.lock_inner();
        inner.lru.contains(path)
    }
}

fn is_decompiled_path(path: &VfsPath) -> bool {
    matches!(
        path,
        VfsPath::Decompiled { .. } | VfsPath::LegacyDecompiled { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH_64: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn ignores_non_decompiled_paths() {
        let store = VirtualDocumentStore::new(1024);
        store.insert_text(VfsPath::local("/tmp/Main.java"), "text".to_string());
        assert!(!store.contains(&VfsPath::local("/tmp/Main.java")));
    }

    #[test]
    fn evicts_lru_entries_to_respect_byte_budget() {
        let store = VirtualDocumentStore::new(12);

        let a = VfsPath::decompiled(HASH_64, "com.example.A");
        let b = VfsPath::decompiled(HASH_64, "com.example.B");
        let c = VfsPath::decompiled(HASH_64, "com.example.C");

        // Each doc is 6 bytes; budget is 12, so at most 2 docs.
        store.insert_text(a.clone(), "aaaaaa".to_string());
        store.insert_text(b.clone(), "bbbbbb".to_string());

        // Touch `a` so that `b` becomes least recently used.
        assert_eq!(store.get_text(&a).as_deref(), Some("aaaaaa"));

        // Insert `c`, forcing eviction of `b`.
        store.insert_text(c.clone(), "cccccc".to_string());

        assert!(store.contains(&a));
        assert!(!store.contains(&b));
        assert!(store.contains(&c));
    }
}
