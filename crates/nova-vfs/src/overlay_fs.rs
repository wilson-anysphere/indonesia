use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};

use nova_core::TextEdit;

use crate::document::{ContentChange, Document, DocumentError};
use crate::fs::FileSystem;
use crate::path::VfsPath;

/// A file system overlay that serves in-memory `Document`s before delegating to a base file system.
#[derive(Debug, Clone)]
pub struct OverlayFs<F: FileSystem> {
    base: F,
    docs: Arc<Mutex<HashMap<VfsPath, Document>>>,
}

impl<F: FileSystem> OverlayFs<F> {
    pub fn new(base: F) -> Self {
        Self {
            base,
            docs: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn open(&self, path: VfsPath, text: String, version: i32) {
        let mut docs = self.docs.lock().expect("overlay mutex poisoned");
        docs.insert(path, Document::new(text, version));
    }

    pub fn close(&self, path: &VfsPath) {
        let mut docs = self.docs.lock().expect("overlay mutex poisoned");
        docs.remove(path);
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

        let mut docs = self.docs.lock().expect("overlay mutex poisoned");
        let Some(doc) = docs.remove(from) else {
            return false;
        };

        match docs.entry(to) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(doc);
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
        let mut docs = self.docs.lock().expect("overlay mutex poisoned");
        let Some(doc) = docs.remove(from) else {
            return;
        };
        docs.insert(to, doc);
    }

    pub fn is_open(&self, path: &VfsPath) -> bool {
        let docs = self.docs.lock().expect("overlay mutex poisoned");
        docs.contains_key(path)
    }

    pub fn apply_changes(
        &self,
        path: &VfsPath,
        new_version: i32,
        changes: &[ContentChange],
    ) -> Result<Vec<TextEdit>, DocumentError> {
        let mut docs = self.docs.lock().expect("overlay mutex poisoned");
        let doc = docs.get_mut(path).ok_or(DocumentError::DocumentNotOpen)?;
        doc.apply_changes(new_version, changes)
    }

    pub fn document_text(&self, path: &VfsPath) -> Option<String> {
        let docs = self.docs.lock().expect("overlay mutex poisoned");
        docs.get(path).map(|d| d.text().to_owned())
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
}
