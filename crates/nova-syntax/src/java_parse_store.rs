use crate::JavaParseResult;
use nova_core::FileId;
use nova_memory::{EvictionRequest, EvictionResult, MemoryCategory, MemoryEvictor, MemoryManager};
use nova_vfs::OpenDocuments;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

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

#[derive(Debug, Clone)]
struct JavaParseEntry {
    text: Arc<String>,
    parse: Arc<JavaParseResult>,
}

/// A memory-pressure aware store of full-fidelity Rowan Java parse results.
///
/// The store pins parse results for open documents so they can be reused even if
/// Salsa memo tables are evicted and recomputed.
///
/// ## Memory accounting
///
/// The store reports best-effort syntax tree usage under
/// [`MemoryCategory::SyntaxTrees`] (approximated as the source text length).
///
/// When used together with Nova's Salsa query database (`nova-db`), the parse
/// result is typically an `Arc<JavaParseResult>` that is shared between Salsa
/// memoization and this store. Callers should avoid counting the same
/// allocation in both places (e.g. by recording `0` bytes for pinned parses in
/// the Salsa memo footprint tracker).
///
/// Entries are keyed by `(FileId, Arc<String>)` using pointer identity: callers
/// must keep the same `Arc<String>` alive for as long as they want cache hits.
#[derive(Debug)]
pub struct JavaParseStore {
    name: String,
    open_docs: Arc<OpenDocuments>,
    inner: Mutex<HashMap<FileId, JavaParseEntry>>,
    on_remove: OnceLock<OnRemoveCallback>,
    registration: OnceLock<nova_memory::MemoryRegistration>,
    tracker: OnceLock<nova_memory::MemoryTracker>,
}

impl JavaParseStore {
    pub fn new(manager: &MemoryManager, open_docs: Arc<OpenDocuments>) -> Arc<Self> {
        let store = Arc::new(Self {
            name: "java_parse_trees".to_string(),
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

    /// Register a callback invoked whenever an entry is removed from the store.
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

    fn prune_closed_files_locked(
        &self,
        inner: &mut HashMap<FileId, JavaParseEntry>,
    ) -> Vec<(FileId, u64)> {
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

    /// Returns the cached parse result for `file` if:
    /// - the document is currently open, and
    /// - the cached text snapshot is the same allocation as `text` (`Arc::ptr_eq`).
    pub fn get_if_text_matches(
        &self,
        file: FileId,
        text: &Arc<String>,
    ) -> Option<Arc<JavaParseResult>> {
        let (cached, removed) = {
            let mut inner = self.inner.lock().unwrap();
            let removed = self.prune_closed_files_locked(&mut inner);

            if !self.open_docs.is_open(file) {
                if !removed.is_empty() {
                    self.update_tracker_locked(&inner);
                }
                (None, removed)
            } else {
                let cached = inner
                    .get(&file)
                    .and_then(|entry| Arc::ptr_eq(&entry.text, text).then(|| entry.parse.clone()));

                if cached.is_some() {
                    if !removed.is_empty() {
                        self.update_tracker_locked(&inner);
                    }
                    (cached, removed)
                } else {
                    // Keep the previous entry even if the text snapshot no longer matches. The
                    // pinned parse can still be used as an incremental reparse base (e.g. across
                    // Salsa memo eviction) and will be replaced on the next successful parse of the
                    // updated text (see `insert`).
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

    /// Insert a parse result for `file` if it is currently open.
    ///
    /// If the file is not open, this removes any existing cached entry for it.
    pub fn insert(&self, file: FileId, text: Arc<String>, parse: Arc<JavaParseResult>) {
        let removed = {
            let mut inner = self.inner.lock().unwrap();
            let mut removed = self.prune_closed_files_locked(&mut inner);

            if !self.open_docs.is_open(file) {
                if !removed.is_empty() {
                    self.update_tracker_locked(&inner);
                }
                removed
            } else {
                if let Some(prev) = inner.insert(file, JavaParseEntry { text, parse }) {
                    // Replacing an existing pinned parse is effectively a removal of the old entry;
                    // notify the callback so callers can update any external memory accounting.
                    removed.push((file, prev.text.len() as u64));
                }
                self.update_tracker_locked(&inner);
                removed
            }
        };
        self.notify_removed(removed);
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
                callback.call(file, removed.text.len() as u64);
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

    pub fn contains(&self, file: FileId) -> bool {
        self.inner.lock().unwrap().contains_key(&file)
    }

    pub fn tracked_bytes(&self) -> u64 {
        self.tracker.get().map(|t| t.bytes()).unwrap_or(0)
    }

    fn clear_all(&self) {
        let removed: Vec<(FileId, u64)> = {
            let mut inner = self.inner.lock().unwrap();
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

    fn update_tracker_locked(&self, inner: &HashMap<FileId, JavaParseEntry>) {
        let Some(tracker) = self.tracker.get() else {
            return;
        };

        // Approximate parse memory by source length.
        let total: u64 = inner.values().map(|entry| entry.text.len() as u64).sum();
        tracker.set_bytes(total);
    }
}

impl MemoryEvictor for JavaParseStore {
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
