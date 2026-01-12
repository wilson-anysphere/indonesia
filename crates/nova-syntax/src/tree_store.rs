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
    registration: OnceLock<nova_memory::MemoryRegistration>,
    tracker: OnceLock<nova_memory::MemoryTracker>,
}

impl SyntaxTreeStore {
    pub fn new(manager: &MemoryManager, open_docs: Arc<OpenDocuments>) -> Arc<Self> {
        let store = Arc::new(Self {
            name: "syntax_trees".to_string(),
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

    pub fn insert(&self, file: FileId, text: Arc<String>, parse: Arc<ParseResult>) {
        // Only keep parses for documents that are currently open; otherwise we'd retain parse
        // results for the entire workspace and duplicate Salsa's memo tables.
        if !self.open_docs.is_open(file) {
            return;
        }

        let mut inner = self.inner.lock().unwrap();
        inner.insert(file, StoredTree { text, parse });
        self.update_tracker_locked(&inner);
    }

    /// Returns the stored parse result if it corresponds to `text`.
    ///
    /// This uses pointer identity (`Arc::ptr_eq`) to avoid expensive hashing.
    pub fn get_if_current(&self, file: FileId, text: &Arc<String>) -> Option<Arc<ParseResult>> {
        let inner = self.inner.lock().unwrap();
        let stored = inner.get(&file)?;
        if Arc::ptr_eq(&stored.text, text) {
            Some(stored.parse.clone())
        } else {
            None
        }
    }

    /// Returns the stored parse result if it corresponds to `text`.
    ///
    /// Alias for [`SyntaxTreeStore::get_if_current`].
    pub fn get_if_text_matches(&self, file: FileId, text: &Arc<String>) -> Option<Arc<ParseResult>> {
        self.get_if_current(file, text)
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

    fn clear_all(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.clear();
        self.update_tracker_locked(&inner);
    }

    fn update_tracker_locked(&self, inner: &HashMap<FileId, StoredTree>) {
        let Some(tracker) = self.tracker.get() else {
            return;
        };
        // Approximate parse memory by source length (stored in the root node).
        let total: u64 = inner.values().map(|stored| stored.parse.root.text_len as u64).sum();
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
