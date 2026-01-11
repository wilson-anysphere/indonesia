use nova_core::FileId;
use std::collections::HashSet;
use std::sync::Mutex;

/// Tracks which documents are currently open in the editor.
///
/// This is used by memory management to keep syntax trees for open files and
/// release trees for closed files under pressure.
#[derive(Debug, Default)]
pub struct OpenDocuments {
    inner: Mutex<HashSet<FileId>>,
}

impl OpenDocuments {
    pub fn open(&self, file: FileId) {
        self.inner
            .lock()
            .expect("open docs mutex poisoned")
            .insert(file);
    }

    pub fn close(&self, file: FileId) {
        self.inner
            .lock()
            .expect("open docs mutex poisoned")
            .remove(&file);
    }

    pub fn is_open(&self, file: FileId) -> bool {
        self.inner
            .lock()
            .expect("open docs mutex poisoned")
            .contains(&file)
    }

    pub fn snapshot(&self) -> HashSet<FileId> {
        self.inner.lock().expect("open docs mutex poisoned").clone()
    }
}
