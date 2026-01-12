use crate::JavaParseResult;
use nova_core::FileId;
use nova_memory::{EvictionRequest, EvictionResult, MemoryCategory, MemoryEvictor, MemoryManager};
use nova_vfs::OpenDocuments;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

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
/// Entries are keyed by `(FileId, Arc<String>)` using pointer identity: callers
/// must keep the same `Arc<String>` alive for as long as they want cache hits.
#[derive(Debug)]
pub struct JavaParseStore {
    name: String,
    open_docs: Arc<OpenDocuments>,
    inner: Mutex<HashMap<FileId, JavaParseEntry>>,
    registration: OnceLock<nova_memory::MemoryRegistration>,
    tracker: OnceLock<nova_memory::MemoryTracker>,
}

impl JavaParseStore {
    pub fn new(manager: &MemoryManager, open_docs: Arc<OpenDocuments>) -> Arc<Self> {
        let store = Arc::new(Self {
            name: "java_parse_trees".to_string(),
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

    /// Returns the cached parse result for `file` if:
    /// - the document is currently open, and
    /// - the cached text snapshot is the same allocation as `text` (`Arc::ptr_eq`).
    pub fn get_if_text_matches(
        &self,
        file: FileId,
        text: &Arc<String>,
    ) -> Option<Arc<JavaParseResult>> {
        let mut inner = self.inner.lock().unwrap();

        if !self.open_docs.is_open(file) {
            if inner.remove(&file).is_some() {
                self.update_tracker_locked(&inner);
            }
            return None;
        }

        inner
            .get(&file)
            .and_then(|entry| Arc::ptr_eq(&entry.text, text).then(|| entry.parse.clone()))
    }

    /// Insert a parse result for `file` if it is currently open.
    ///
    /// If the file is not open, this removes any existing cached entry for it.
    pub fn insert(&self, file: FileId, text: Arc<String>, parse: Arc<JavaParseResult>) {
        let mut inner = self.inner.lock().unwrap();

        // Opportunistically drop closed docs whenever we touch the store.
        inner.retain(|file, _| self.open_docs.is_open(*file));

        if !self.open_docs.is_open(file) {
            self.update_tracker_locked(&inner);
            return;
        }

        inner.insert(file, JavaParseEntry { text, parse });
        self.update_tracker_locked(&inner);
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

