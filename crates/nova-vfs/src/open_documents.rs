use nova_core::FileId;
use std::collections::HashSet;
use std::sync::{Mutex, MutexGuard};

/// Tracks which documents are currently open in the editor.
///
/// This is used by memory management to keep syntax trees for open files and
/// release trees for closed files under pressure.
#[derive(Debug, Default)]
pub struct OpenDocuments {
    inner: Mutex<HashSet<FileId>>,
}

impl OpenDocuments {
    #[track_caller]
    fn lock_inner(&self) -> MutexGuard<'_, HashSet<FileId>> {
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

    pub fn open(&self, file: FileId) {
        self.lock_inner().insert(file);
    }

    pub fn close(&self, file: FileId) {
        self.lock_inner().remove(&file);
    }

    pub fn is_open(&self, file: FileId) -> bool {
        self.lock_inner().contains(&file)
    }

    pub fn snapshot(&self) -> HashSet<FileId> {
        self.lock_inner().clone()
    }
}
