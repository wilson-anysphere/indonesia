use crate::ParseResult;
use nova_core::FileId;
use nova_memory::{EvictionRequest, EvictionResult, MemoryCategory, MemoryEvictor, MemoryManager};
use nova_vfs::OpenDocuments;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

/// A memory-pressure aware store of parsed syntax trees.
///
/// The store keeps trees for open documents and opportunistically releases trees
/// for closed files under memory pressure.
#[derive(Debug)]
pub struct SyntaxTreeStore {
    name: String,
    open_docs: Arc<OpenDocuments>,
    inner: Mutex<HashMap<FileId, Arc<ParseResult>>>,
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

        let registration =
            manager.register_evictor(store.name.clone(), MemoryCategory::SyntaxTrees, store.clone());
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

    pub fn insert(&self, file: FileId, parse: Arc<ParseResult>) {
        let mut inner = self.inner.lock().unwrap();
        inner.insert(file, parse);
        self.update_tracker_locked(&inner);
    }

    pub fn get(&self, file: FileId) -> Option<Arc<ParseResult>> {
        self.inner.lock().unwrap().get(&file).cloned()
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

    fn update_tracker_locked(&self, inner: &HashMap<FileId, Arc<ParseResult>>) {
        let Some(tracker) = self.tracker.get() else { return };
        // Approximate parse memory by source length (stored in the root node).
        let total: u64 = inner
            .values()
            .map(|parse| parse.root.text_len as u64)
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

