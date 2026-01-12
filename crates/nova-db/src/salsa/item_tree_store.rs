use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use nova_core::FileId;
use nova_hir::token_item_tree::TokenItemTree;
use nova_memory::{EvictionRequest, EvictionResult, MemoryCategory, MemoryEvictor, MemoryManager};
use nova_vfs::OpenDocuments;

#[derive(Debug, Clone)]
struct Entry {
    text: Arc<String>,
    item_tree: Arc<TokenItemTree>,
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
    registration: OnceLock<nova_memory::MemoryRegistration>,
    tracker: OnceLock<nova_memory::MemoryTracker>,
}

impl ItemTreeStore {
    pub fn new(manager: &MemoryManager, open_docs: Arc<OpenDocuments>) -> Arc<Self> {
        let store = Arc::new(Self {
            name: "item_trees".to_string(),
            open_docs,
            inner: Mutex::new(HashMap::new()),
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

    pub fn is_open(&self, file: FileId) -> bool {
        self.open_docs.is_open(file)
    }

    /// Returns a cached `item_tree` result if `file` is open and `text` matches
    /// by pointer identity.
    pub fn get_if_text_matches(
        &self,
        file: FileId,
        text: &Arc<String>,
    ) -> Option<Arc<TokenItemTree>> {
        let mut inner = self.inner.lock().unwrap();

        // Opportunistically drop closed documents so the store only retains
        // items for currently-open files.
        let len_before = inner.len();
        inner.retain(|file, _| self.open_docs.is_open(*file));
        if inner.len() != len_before {
            self.update_tracker_locked(&inner);
        }

        if !self.open_docs.is_open(file) {
            return None;
        }

        let entry = inner.get(&file)?;
        if Arc::ptr_eq(&entry.text, text) {
            return Some(entry.item_tree.clone());
        }

        // Stale entry (file still open but text changed). Drop it now so the
        // next computed tree can replace it.
        inner.remove(&file);
        self.update_tracker_locked(&inner);
        None
    }

    pub fn insert(&self, file: FileId, text: Arc<String>, item_tree: Arc<TokenItemTree>) {
        let mut inner = self.inner.lock().unwrap();
        let len_before = inner.len();
        inner.retain(|file, _| self.open_docs.is_open(*file));

        if !self.open_docs.is_open(file) {
            if inner.len() != len_before {
                self.update_tracker_locked(&inner);
            }
            return;
        }

        inner.insert(file, Entry { text, item_tree });
        self.update_tracker_locked(&inner);
    }

    pub fn remove(&self, file: FileId) {
        let mut inner = self.inner.lock().unwrap();
        if inner.remove(&file).is_some() {
            self.update_tracker_locked(&inner);
        }
    }

    pub fn release_closed_files(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.retain(|file, _| self.open_docs.is_open(*file));
        self.update_tracker_locked(&inner);
    }

    pub fn contains(&self, file: FileId) -> bool {
        self.inner.lock().unwrap().contains_key(&file)
    }

    pub fn tracked_bytes(&self) -> u64 {
        self.tracker.get().map(|t| t.bytes()).unwrap_or(0)
    }

    fn clear_all(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.clear();
        self.update_tracker_locked(&inner);
    }

    fn update_tracker_locked(&self, inner: &HashMap<FileId, Entry>) {
        let Some(tracker) = self.tracker.get() else {
            return;
        };
        // Approximate memory by source length.
        let total: u64 = inner.values().map(|entry| entry.text.len() as u64).sum();
        tracker.set_bytes(total);
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
