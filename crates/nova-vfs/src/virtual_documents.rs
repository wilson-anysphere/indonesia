use std::sync::{Arc, Mutex};

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

    /// Inserts a UTF-8 document into the store.
    ///
    /// Non-decompiled paths are ignored.
    pub fn insert_text(&self, path: VfsPath, text: String) {
        if !is_decompiled_path(&path) {
            return;
        }

        let text: Arc<str> = Arc::from(text);
        let bytes = text.len();

        let mut inner = self
            .inner
            .lock()
            .expect("virtual document store mutex poisoned");

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

        let mut inner = self
            .inner
            .lock()
            .expect("virtual document store mutex poisoned");
        inner.lru.get(path).cloned()
    }

    /// Returns whether the store contains an entry for `path`.
    ///
    /// Non-decompiled paths always return `false`.
    pub fn contains(&self, path: &VfsPath) -> bool {
        if !is_decompiled_path(path) {
            return false;
        }

        let inner = self
            .inner
            .lock()
            .expect("virtual document store mutex poisoned");
        inner.lru.contains(path)
    }
}

fn is_decompiled_path(path: &VfsPath) -> bool {
    matches!(
        path,
        VfsPath::Decompiled { .. } | VfsPath::LegacyDecompiled { .. }
    )
}
