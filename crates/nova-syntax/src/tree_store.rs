use crate::ParseResult;
use nova_core::FileId;
use nova_memory::{EvictionRequest, EvictionResult, MemoryCategory, MemoryEvictor, MemoryManager};
use nova_vfs::OpenDocuments;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Debug, Clone)]
struct StoredTree {
    /// Source text snapshot used to produce `parse`.
    ///
    /// This is used to ensure we never return a stale parse for a file whose
    /// `file_content` Salsa input has been updated.
    text: Arc<String>,
    parse: Arc<ParseResult>,
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

/// A memory-pressure aware store of parsed syntax trees.
///
/// The store keeps trees for open documents and opportunistically releases trees
/// for closed files under memory pressure.
///
/// ## Memory accounting
///
/// The store reports approximate syntax tree memory usage under
/// [`MemoryCategory::SyntaxTrees`] (approximated as the source text length).
///
/// When used together with Nova's Salsa query database (`nova-db`), the parse
/// result is typically an `Arc<ParseResult>` that is shared between Salsa
/// memoization and this store. Callers should avoid counting the same allocation
/// in both places (e.g. by recording `0` bytes for pinned parses in the Salsa
/// memo footprint tracker).
#[derive(Debug)]
pub struct SyntaxTreeStore {
    name: String,
    open_docs: Arc<OpenDocuments>,
    inner: Mutex<HashMap<FileId, StoredTree>>,
    on_remove: OnceLock<OnRemoveCallback>,
    registration: OnceLock<nova_memory::MemoryRegistration>,
    tracker: OnceLock<nova_memory::MemoryTracker>,
}

impl SyntaxTreeStore {
    pub fn new(manager: &MemoryManager, open_docs: Arc<OpenDocuments>) -> Arc<Self> {
        let store = Arc::new(Self {
            name: "syntax_trees".to_string(),
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

    /// Register a callback invoked whenever a stored tree is removed from the store.
    ///
    /// This is intended for integration with `nova-db`'s Salsa memo footprint tracking:
    /// when a pinned parse result is removed, memory accounting should attribute the
    /// allocation back to Salsa memo tables (to avoid undercounting).
    pub fn set_on_remove(&self, callback: Arc<dyn Fn(FileId, u64) + Send + Sync>) {
        let _ = self.on_remove.set(OnRemoveCallback(callback));
    }

    pub fn is_open(&self, file: FileId) -> bool {
        self.open_docs.is_open(file)
    }

    fn prune_closed_files_locked(&self, inner: &mut HashMap<FileId, StoredTree>) -> Vec<(FileId, u64)> {
        let mut removed = Vec::new();
        inner.retain(|file, stored| {
            let keep = self.open_docs.is_open(*file);
            if !keep {
                removed.push((*file, stored.parse.root.text_len as u64));
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

    pub fn insert(&self, file: FileId, text: Arc<String>, parse: Arc<ParseResult>) {
        let removed = {
            let mut inner = self.inner.lock().unwrap();
            let removed = self.prune_closed_files_locked(&mut inner);

            // Only keep parses for documents that are currently open; otherwise we'd retain parse
            // results for the entire workspace and duplicate Salsa's memo tables.
            if !self.open_docs.is_open(file) {
                if !removed.is_empty() {
                    self.update_tracker_locked(&inner);
                }
                removed
            } else {
                inner.insert(file, StoredTree { text, parse });
                self.update_tracker_locked(&inner);
                removed
            }
        };
        self.notify_removed(removed);
    }

    /// Returns the stored parse result if it corresponds to `text`.
    ///
    /// This uses pointer identity (`Arc::ptr_eq`) to avoid expensive hashing.
    pub fn get_if_current(&self, file: FileId, text: &Arc<String>) -> Option<Arc<ParseResult>> {
        let (cached, removed) = {
            let mut inner = self.inner.lock().unwrap();
            let mut removed = self.prune_closed_files_locked(&mut inner);

            if !self.open_docs.is_open(file) {
                if !removed.is_empty() {
                    self.update_tracker_locked(&inner);
                }
                (None, removed)
            } else {
                let cached = inner.get(&file).and_then(|stored| {
                    Arc::ptr_eq(&stored.text, text).then(|| stored.parse.clone())
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
                        removed.push((file, entry.parse.root.text_len as u64));
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

    /// Returns the stored parse result if it corresponds to `text`.
    ///
    /// Alias for [`SyntaxTreeStore::get_if_current`].
    pub fn get_if_text_matches(
        &self,
        file: FileId,
        text: &Arc<String>,
    ) -> Option<Arc<ParseResult>> {
        self.get_if_current(file, text)
    }

    pub fn contains(&self, file: FileId) -> bool {
        self.inner.lock().unwrap().contains_key(&file)
    }

    pub fn remove(&self, file: FileId) {
        let removed = {
            let mut inner = self.inner.lock().unwrap();
            let removed = inner.remove(&file);
            if removed.is_some() {
                self.update_tracker_locked(&inner);
            }
            removed
        };
        if let Some(removed) = removed {
            if let Some(callback) = self.on_remove.get() {
                callback.call(file, removed.parse.root.text_len as u64);
            }
        }
    }

    pub fn release_closed_files(&self) {
        let removed = {
            let mut inner = self.inner.lock().unwrap();
            let removed = self.prune_closed_files_locked(&mut inner);
            if !removed.is_empty() {
                self.update_tracker_locked(&inner);
            }
            removed
        };
        self.notify_removed(removed);
    }

    pub fn tracked_bytes(&self) -> u64 {
        self.tracker.get().map(|t| t.bytes()).unwrap_or(0)
    }

    fn clear_all(&self) {
        let removed: Vec<(FileId, u64)> = {
            let mut inner = self.inner.lock().unwrap();
            let removed = inner
                .iter()
                .map(|(file, stored)| (*file, stored.parse.root.text_len as u64))
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

    fn update_tracker_locked(&self, inner: &HashMap<FileId, StoredTree>) {
        let Some(tracker) = self.tracker.get() else {
            return;
        };
        // Approximate parse memory by source length (stored in the root node).
        let total: u64 = inner
            .values()
            .map(|stored| stored.parse.root.text_len as u64)
            .sum();
        tracker.set_bytes(total);
    }
}

impl MemoryEvictor for SyntaxTreeStore {
    fn name(&self) -> &str {
        &self.name
    }

    fn category(&self) -> MemoryCategory {
        MemoryCategory::SyntaxTrees
    }

    fn evict(&self, request: EvictionRequest) -> EvictionResult {
        let before = self.tracker.get().map(|t| t.bytes()).unwrap_or(0);

        match request.pressure {
            nova_memory::MemoryPressure::Low | nova_memory::MemoryPressure::Medium => {
                self.release_closed_files();
            }
            nova_memory::MemoryPressure::High => {
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
