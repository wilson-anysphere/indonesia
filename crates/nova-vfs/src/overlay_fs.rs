use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex, MutexGuard};

use nova_core::TextEdit;

use crate::document::{ContentChange, Document, DocumentError};
use crate::fs::FileSystem;
use crate::path::VfsPath;

#[derive(Debug, Default)]
struct OverlayDocs {
    docs: HashMap<VfsPath, Document>,
    /// Best-effort accounting of UTF-8 document bytes currently stored in the overlay.
    ///
    /// This tracks `Document::text().len()` (not the `String` capacity) so callers can
    /// deterministically test and report approximate usage without scanning the map.
    text_bytes: usize,
}

/// A file system overlay that serves in-memory `Document`s before delegating to a base file system.
#[derive(Debug, Clone)]
pub struct OverlayFs<F: FileSystem> {
    base: F,
    docs: Arc<Mutex<OverlayDocs>>,
}

impl<F: FileSystem> OverlayFs<F> {
    fn normalize_if_needed(&self, path: &VfsPath) -> Option<VfsPath> {
        let normalized = crate::path::normalize_vfs_path(path.clone());
        if &normalized == path {
            None
        } else {
            Some(normalized)
        }
    }

    pub fn new(base: F) -> Self {
        Self {
            base,
            docs: Arc::new(Mutex::new(OverlayDocs::default())),
        }
    }

    #[track_caller]
    fn lock_docs(&self) -> MutexGuard<'_, OverlayDocs> {
        match self.docs.lock() {
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

    pub fn open_arc(&self, path: VfsPath, text: Arc<String>, version: i32) {
        let bytes = text.len();
        let mut state = self.lock_docs();

        // Ensure we only keep one entry for logically-equivalent paths (e.g. a `file:` URI vs a
        // local path, or dot-segment differences).
        let normalized = crate::path::normalize_vfs_path(path.clone());
        if normalized != path {
            if let Some(old) = state.docs.remove(&path) {
                state.text_bytes = state.text_bytes.saturating_sub(old.text().len());
            }
        }

        let old = state.docs.insert(normalized, Document::new(text, version));
        if let Some(old) = old {
            state.text_bytes = state.text_bytes.saturating_sub(old.text().len());
        }
        state.text_bytes = state.text_bytes.saturating_add(bytes);
    }

    pub fn open(&self, path: VfsPath, text: String, version: i32) {
        self.open_arc(path, Arc::new(text), version);
    }

    pub fn close(&self, path: &VfsPath) {
        let mut state = self.lock_docs();
        if let Some(doc) = state.docs.remove(path) {
            state.text_bytes = state.text_bytes.saturating_sub(doc.text().len());
            return;
        }
        if let Some(normalized) = self.normalize_if_needed(path) {
            if let Some(doc) = state.docs.remove(&normalized) {
                state.text_bytes = state.text_bytes.saturating_sub(doc.text().len());
            }
        }
    }

    /// Best-effort estimate of the total number of UTF-8 bytes stored in open overlay documents.
    ///
    /// This is intended for coarse memory accounting (e.g. `nova_memory`) and is updated
    /// incrementally on open/close/edit/rename operations.
    pub fn estimated_bytes(&self) -> usize {
        let state = self.lock_docs();
        state.text_bytes
    }

    /// Renames an open document in the overlay from `from` to `to`.
    ///
    /// Returns `true` if a document was moved, `false` otherwise.
    ///
    /// If `to` is already open, the destination document is kept and the source
    /// document (if any) is dropped.
    pub fn rename(&self, from: &VfsPath, to: VfsPath) -> bool {
        if from == &to {
            return false;
        }

        let mut state = self.lock_docs();
        let (from_key, doc) = if let Some(doc) = state.docs.remove(from) {
            (from.clone(), doc)
        } else if let Some(normalized) = self.normalize_if_needed(from) {
            let Some(doc) = state.docs.remove(&normalized) else {
                return false;
            };
            (normalized, doc)
        } else {
            return false;
        };
        let bytes = doc.text().len();
        state.text_bytes = state.text_bytes.saturating_sub(bytes);

        let to_key = crate::path::normalize_vfs_path(to.clone());
        if from_key == to_key {
            // No-op after normalization; restore the document.
            state.docs.insert(from_key, doc);
            state.text_bytes = state.text_bytes.saturating_add(bytes);
            return false;
        }

        if state.docs.contains_key(&to) || state.docs.contains_key(&to_key) {
            // Destination is already open; keep it and drop the source doc.
            return false;
        }

        match state.docs.entry(to_key) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(doc);
                state.text_bytes = state.text_bytes.saturating_add(bytes);
                true
            }
            std::collections::hash_map::Entry::Occupied(_) => false,
        }
    }

    /// Rename a document, overwriting any document already open at the destination path.
    ///
    /// This is primarily used to keep the overlay consistent when a file is renamed/moved on disk
    /// while it is open in the editor.
    pub fn rename_overwrite(&self, from: &VfsPath, to: VfsPath) {
        let mut state = self.lock_docs();
        let (from_key, doc) = if let Some(doc) = state.docs.remove(from) {
            (from.clone(), doc)
        } else if let Some(normalized) = self.normalize_if_needed(from) {
            let Some(doc) = state.docs.remove(&normalized) else {
                return;
            };
            (normalized, doc)
        } else {
            return;
        };

        let bytes = doc.text().len();
        state.text_bytes = state.text_bytes.saturating_sub(bytes);

        let to_key = crate::path::normalize_vfs_path(to.clone());
        if from_key == to_key {
            // No-op after normalization; restore the document.
            state.docs.insert(from_key, doc);
            state.text_bytes = state.text_bytes.saturating_add(bytes);
            return;
        }

        if to_key != to {
            if let Some(old) = state.docs.remove(&to) {
                state.text_bytes = state.text_bytes.saturating_sub(old.text().len());
            }
        }

        let old = state.docs.insert(to_key, doc);
        state.text_bytes = state.text_bytes.saturating_add(bytes);
        if let Some(old) = old {
            state.text_bytes = state.text_bytes.saturating_sub(old.text().len());
        }
    }

    pub fn is_open(&self, path: &VfsPath) -> bool {
        let state = self.lock_docs();
        if state.docs.contains_key(path) {
            return true;
        }
        let Some(normalized) = self.normalize_if_needed(path) else {
            return false;
        };
        state.docs.contains_key(&normalized)
    }

    pub fn apply_changes(
        &self,
        path: &VfsPath,
        new_version: i32,
        changes: &[ContentChange],
    ) -> Result<Vec<TextEdit>, DocumentError> {
        let mut state = self.lock_docs();
        let doc = if state.docs.contains_key(path) {
            state
                .docs
                .get_mut(path)
                .expect("contains_key checked before get_mut")
        } else if let Some(normalized) = self.normalize_if_needed(path) {
            state
                .docs
                .get_mut(&normalized)
                .ok_or(DocumentError::DocumentNotOpen)?
        } else {
            return Err(DocumentError::DocumentNotOpen);
        };

        let before = doc.text().len();
        let result = doc.apply_changes(new_version, changes);
        let after = doc.text().len();

        if after >= before {
            state.text_bytes = state.text_bytes.saturating_add(after - before);
        } else {
            state.text_bytes = state.text_bytes.saturating_sub(before - after);
        }

        result
    }

    pub fn document_text(&self, path: &VfsPath) -> Option<String> {
        let state = self.lock_docs();
        if let Some(doc) = state.docs.get(path) {
            return Some(doc.text().to_owned());
        }
        let normalized = self.normalize_if_needed(path)?;
        state.docs.get(&normalized).map(|d| d.text().to_owned())
    }

    pub fn document_text_arc(&self, path: &VfsPath) -> Option<Arc<String>> {
        let state = self.lock_docs();
        if let Some(doc) = state.docs.get(path) {
            return Some(Document::text_arc(doc));
        }
        let normalized = self.normalize_if_needed(path)?;
        state.docs.get(&normalized).map(Document::text_arc)
    }
}

impl<F: FileSystem> FileSystem for OverlayFs<F> {
    fn read_bytes(&self, path: &VfsPath) -> io::Result<Vec<u8>> {
        if let Some(text) = self.document_text(path) {
            return Ok(text.into_bytes());
        }
        self.base.read_bytes(path)
    }

    fn read_to_string(&self, path: &VfsPath) -> io::Result<String> {
        if let Some(text) = self.document_text(path) {
            return Ok(text);
        }
        self.base.read_to_string(path)
    }

    fn exists(&self, path: &VfsPath) -> bool {
        if self.is_open(path) {
            return true;
        }
        self.base.exists(path)
    }

    fn metadata(&self, path: &VfsPath) -> io::Result<std::fs::Metadata> {
        // For open documents we still return the underlying file metadata.
        self.base.metadata(path)
    }

    fn read_dir(&self, path: &VfsPath) -> io::Result<Vec<VfsPath>> {
        self.base.read_dir(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;

    use crate::fs::LocalFs;

    #[test]
    fn overlay_precedence_over_disk() {
        let temp = tempfile::tempdir().unwrap();
        let file_path = temp.path().join("Main.java");
        fs::write(&file_path, "disk").unwrap();
        let vfs_path = VfsPath::local(file_path.clone());

        let overlay = OverlayFs::new(LocalFs::new());
        assert_eq!(overlay.read_to_string(&vfs_path).unwrap(), "disk");

        overlay.open(vfs_path.clone(), "overlay".to_string(), 1);
        assert_eq!(overlay.read_to_string(&vfs_path).unwrap(), "overlay");

        overlay.close(&vfs_path);
        assert_eq!(overlay.read_to_string(&vfs_path).unwrap(), "disk");
    }

    #[test]
    fn estimated_bytes_tracks_overlay_document_text() {
        let dir = tempfile::tempdir().unwrap();
        let overlay = OverlayFs::new(LocalFs::new());

        let a = VfsPath::local(dir.path().join("a.java"));
        let b = VfsPath::local(dir.path().join("b.java"));
        let c = VfsPath::local(dir.path().join("c.java"));

        overlay.open(a.clone(), "aaa".to_string(), 1);
        overlay.open(b.clone(), "bbbb".to_string(), 1);
        assert_eq!(overlay.estimated_bytes(), 7);

        overlay
            .apply_changes(&b, 2, &[ContentChange::full("bbbbbb")])
            .unwrap();
        assert_eq!(overlay.estimated_bytes(), 9);

        overlay.close(&a);
        assert_eq!(overlay.estimated_bytes(), 6);

        overlay.open(c.clone(), "c".to_string(), 1);
        assert_eq!(overlay.estimated_bytes(), 7);

        // Renaming onto an already-open destination drops the source document.
        assert!(!overlay.rename(&b, c.clone()));
        assert!(!overlay.is_open(&b));
        assert_eq!(overlay.document_text(&c).unwrap(), "c");
        assert_eq!(overlay.estimated_bytes(), 1);
    }

    #[test]
    fn overlay_normalizes_paths_for_lookup_and_close() {
        let temp = tempfile::tempdir().unwrap();
        let file_path = temp.path().join("x").join("..").join("Main.java");

        let unnormalized = VfsPath::Local(file_path);
        let normalized = crate::path::normalize_vfs_path(unnormalized.clone());

        let overlay = OverlayFs::new(LocalFs::new());
        overlay.open(unnormalized.clone(), "overlay".to_string(), 1);

        assert!(overlay.is_open(&unnormalized));
        assert!(overlay.is_open(&normalized));
        assert_eq!(overlay.read_to_string(&normalized).unwrap(), "overlay");

        overlay.close(&normalized);
        assert!(!overlay.is_open(&unnormalized));
        assert!(!overlay.is_open(&normalized));
    }
}
