use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use nova_core::FileId;
use nova_hir::token_item_tree::TokenItemTree;
use nova_memory::{EvictionRequest, EvictionResult, MemoryCategory, MemoryEvictor, MemoryManager};
use nova_vfs::OpenDocuments;

#[derive(Debug, Clone)]
struct Entry {
    text: Arc<String>,
    item_tree: Arc<TokenItemTree>,
}

type OnRemoveFn = dyn Fn(FileId, u64) + Send + Sync;

struct OnRemoveCallback(Arc<OnRemoveFn>);

impl OnRemoveCallback {
    fn call(&self, file: FileId, bytes: u64) {
        (self.0)(file, bytes);
    }
}

impl std::fmt::Debug for OnRemoveCallback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("OnRemoveCallback")
            .field(&"<callback>")
            .finish()
    }
}

/// A memory-pressure aware store for Salsa `item_tree` results.
///
/// This store exists to "pin" expensive per-file semantic summaries for open
/// documents across Salsa memo eviction (which rebuilds the DB and drops
/// memoized query results).
///
/// ## Why this is categorized as `SyntaxTrees`
///
/// `TokenItemTree` is a structural summary derived directly from the token
/// stream and is most closely associated with the per-file syntax pipeline. By
/// registering under [`MemoryCategory::SyntaxTrees`], we avoid the store being
/// evicted alongside Salsa memo tables (which are tracked under
/// `MemoryCategory::QueryCache`).
#[derive(Debug)]
pub struct ItemTreeStore {
    name: String,
    open_docs: Arc<OpenDocuments>,
    inner: Mutex<HashMap<FileId, Entry>>,
    on_remove: OnceLock<OnRemoveCallback>,
    registration: OnceLock<nova_memory::MemoryRegistration>,
    tracker: OnceLock<nova_memory::MemoryTracker>,
}

impl ItemTreeStore {
    pub fn new(manager: &MemoryManager, open_docs: Arc<OpenDocuments>) -> Arc<Self> {
        let store = Arc::new(Self {
            name: "item_trees".to_string(),
            open_docs,
            inner: Mutex::new(HashMap::new()),
            on_remove: OnceLock::new(),
            registration: OnceLock::new(),
            tracker: OnceLock::new(),
        });

        let registration = manager.register_evictor(
            store.name.clone(),
            MemoryCategory::SyntaxTrees,
            store.clone(),
        );
        store
            .tracker
            .set(registration.tracker())
            .expect("tracker only set once");
        store
            .registration
            .set(registration)
            .expect("registration only set once");

        store
    }

    /// Register a callback invoked whenever a stored item tree is removed from the store.
    ///
    /// This is intended for integration with Nova's Salsa memo footprint tracking:
    /// when a pinned item tree is removed, memory accounting should attribute the
    /// allocation back to Salsa memo tables (to avoid undercounting).
    pub fn set_on_remove(&self, callback: Arc<dyn Fn(FileId, u64) + Send + Sync>) {
        let _ = self.on_remove.set(OnRemoveCallback(callback));
    }

    pub fn is_open(&self, file: FileId) -> bool {
        self.open_docs.is_open(file)
    }

    fn prune_closed_files_locked(&self, inner: &mut HashMap<FileId, Entry>) -> Vec<(FileId, u64)> {
        let mut removed = Vec::new();
        inner.retain(|file, entry| {
            let keep = self.open_docs.is_open(*file);
            if !keep {
                removed.push((*file, entry.text.len() as u64));
            }
            keep
        });
        removed
    }

    fn notify_removed(&self, removed: Vec<(FileId, u64)>) {
        if removed.is_empty() {
            return;
        }
        if let Some(callback) = self.on_remove.get() {
            for (file, bytes) in removed {
                callback.call(file, bytes);
            }
        }
    }

    /// Returns a cached `item_tree` result if `file` is open and `text` matches
    /// by pointer identity.
    pub fn get_if_text_matches(
        &self,
        file: FileId,
        text: &Arc<String>,
    ) -> Option<Arc<TokenItemTree>> {
        let (cached, removed) = {
            let mut inner = self.lock_inner();
            let mut removed = self.prune_closed_files_locked(&mut inner);

            if !self.open_docs.is_open(file) {
                if !removed.is_empty() {
                    self.update_tracker_locked(&inner);
                }
                (None, removed)
            } else {
                let cached = inner.get(&file).and_then(|entry| {
                    Arc::ptr_eq(&entry.text, text).then(|| entry.item_tree.clone())
                });
                if cached.is_some() {
                    if !removed.is_empty() {
                        self.update_tracker_locked(&inner);
                    }
                    (cached, removed)
                } else {
                    // Stale entry (file still open but text changed). Drop it now so the
                    // next computed tree can replace it.
                    let removed_entry = inner.remove(&file);
                    if let Some(entry) = removed_entry {
                        removed.push((file, entry.text.len() as u64));
                    }
                    if !removed.is_empty() {
                        self.update_tracker_locked(&inner);
                    }
                    (None, removed)
                }
            }
        };
        self.notify_removed(removed);
        cached
    }

    pub fn insert(&self, file: FileId, text: Arc<String>, item_tree: Arc<TokenItemTree>) {
        let removed = {
            let mut inner = self.lock_inner();
            let removed = self.prune_closed_files_locked(&mut inner);

            if !self.open_docs.is_open(file) {
                if !removed.is_empty() {
                    self.update_tracker_locked(&inner);
                }
                removed
            } else {
                inner.insert(file, Entry { text, item_tree });
                self.update_tracker_locked(&inner);
                removed
            }
        };
        self.notify_removed(removed);
    }

    pub fn remove(&self, file: FileId) {
        let removed = {
            let mut inner = self.lock_inner();
            let removed = inner.remove(&file);
            if removed.is_some() {
                self.update_tracker_locked(&inner);
            }
            removed
        };

        if let Some(removed) = removed {
            if let Some(callback) = self.on_remove.get() {
                callback.call(file, removed.text.len() as u64);
            }
        }
    }

    pub fn release_closed_files(&self) {
        let removed = {
            let mut inner = self.lock_inner();
            let removed = self.prune_closed_files_locked(&mut inner);
            if !removed.is_empty() {
                self.update_tracker_locked(&inner);
            }
            removed
        };
        self.notify_removed(removed);
    }

    pub fn contains(&self, file: FileId) -> bool {
        self.lock_inner().contains_key(&file)
    }

    pub fn tracked_bytes(&self) -> u64 {
        self.tracker.get().map(|t| t.bytes()).unwrap_or(0)
    }

    fn clear_all(&self) {
        let removed: Vec<(FileId, u64)> = {
            let mut inner = self.lock_inner();
            let removed = inner
                .iter()
                .map(|(file, entry)| (*file, entry.text.len() as u64))
                .collect();
            inner.clear();
            self.update_tracker_locked(&inner);
            removed
        };

        if let Some(callback) = self.on_remove.get() {
            for (file, bytes) in removed {
                callback.call(file, bytes);
            }
        }
    }

    fn update_tracker_locked(&self, inner: &HashMap<FileId, Entry>) {
        let Some(tracker) = self.tracker.get() else {
            return;
        };
        // Approximate memory by source length.
        let total: u64 = inner.values().map(|entry| entry.text.len() as u64).sum();
        tracker.set_bytes(total);
    }

    #[track_caller]
    fn lock_inner(&self) -> MutexGuard<'_, HashMap<FileId, Entry>> {
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
}

impl MemoryEvictor for ItemTreeStore {
    fn name(&self) -> &str {
        &self.name
    }

    fn category(&self) -> MemoryCategory {
        MemoryCategory::SyntaxTrees
    }

    fn evict(&self, request: EvictionRequest) -> EvictionResult {
        let before = self.tracker.get().map(|t| t.bytes()).unwrap_or(0);

        match request.pressure {
            nova_memory::MemoryPressure::Low
            | nova_memory::MemoryPressure::Medium
            | nova_memory::MemoryPressure::High => {
                self.release_closed_files();
            }
            nova_memory::MemoryPressure::Critical => {
                self.clear_all();
            }
        }

        let after = self.tracker.get().map(|t| t.bytes()).unwrap_or(0);
        EvictionResult {
            before_bytes: before,
            after_bytes: after,
        }
    }
}
