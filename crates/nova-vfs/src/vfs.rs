use std::io;
use std::sync::{Arc, Mutex};

use nova_core::FileId;

use crate::change::ChangeEvent;
use crate::document::{ContentChange, DocumentError};
use crate::file_id::FileIdRegistry;
use crate::fs::FileSystem;
use crate::open_documents::OpenDocuments;
use crate::overlay_fs::OverlayFs;
use crate::path::VfsPath;

/// High-level VFS facade combining:
/// - a base `FileSystem` (usually `LocalFs`)
/// - an in-memory overlay for open documents (`OverlayFs`)
/// - stable `FileId` allocation (`FileIdRegistry`)
#[derive(Debug, Clone)]
pub struct Vfs<F: FileSystem> {
    fs: OverlayFs<F>,
    ids: Arc<Mutex<FileIdRegistry>>,
    open_docs: Arc<OpenDocuments>,
}

impl<F: FileSystem> Vfs<F> {
    pub fn new(base: F) -> Self {
        Self {
            fs: OverlayFs::new(base),
            ids: Arc::new(Mutex::new(FileIdRegistry::new())),
            open_docs: Arc::new(OpenDocuments::default()),
        }
    }

    pub fn overlay(&self) -> &OverlayFs<F> {
        &self.fs
    }

    /// Returns a shared handle to the set of open document ids.
    pub fn open_documents(&self) -> Arc<OpenDocuments> {
        self.open_docs.clone()
    }

    /// Returns the stable id for `path`, allocating one if needed.
    pub fn file_id(&self, path: VfsPath) -> FileId {
        let mut ids = self.ids.lock().expect("file id registry mutex poisoned");
        ids.file_id(path)
    }

    /// Returns the id for `path` if it has been interned.
    pub fn get_id(&self, path: &VfsPath) -> Option<FileId> {
        let ids = self.ids.lock().expect("file id registry mutex poisoned");
        ids.get_id(path)
    }

    /// Reverse lookup for an interned file id.
    pub fn path_for_id(&self, id: FileId) -> Option<VfsPath> {
        let ids = self.ids.lock().expect("file id registry mutex poisoned");
        ids.get_path(id).cloned()
    }

    /// Rename (or move) a path, preserving the existing `FileId` when possible.
    pub fn rename_path(&self, from: &VfsPath, to: VfsPath) -> FileId {
        let mut ids = self.ids.lock().expect("file id registry mutex poisoned");
        ids.rename_path(from, to)
    }

    /// Returns all currently-tracked file ids (sorted).
    pub fn all_file_ids(&self) -> Vec<FileId> {
        let ids = self.ids.lock().expect("file id registry mutex poisoned");
        ids.all_file_ids()
    }

    /// Opens an in-memory overlay document and returns its `FileId`.
    pub fn open_document(&self, path: VfsPath, text: String, version: i32) -> FileId {
        let id = self.file_id(path.clone());
        self.fs.open(path, text, version);
        self.open_docs.open(id);
        id
    }

    pub fn close_document(&self, path: &VfsPath) {
        if let Some(id) = self.get_id(path) {
            self.open_docs.close(id);
        }
        self.fs.close(path);
    }

    /// Applies document edits and returns a `ChangeEvent` describing the update.
    pub fn apply_document_changes(
        &self,
        path: &VfsPath,
        new_version: i32,
        changes: &[ContentChange],
    ) -> Result<ChangeEvent, DocumentError> {
        let file_id = self.file_id(path.clone());
        let edits = self.fs.apply_changes(path, new_version, changes)?;
        Ok(ChangeEvent::DocumentChanged {
            file_id,
            path: path.clone(),
            version: new_version,
            edits,
        })
    }
}

impl<F: FileSystem> FileSystem for Vfs<F> {
    fn read_to_string(&self, path: &VfsPath) -> io::Result<String> {
        self.fs.read_to_string(path)
    }

    fn exists(&self, path: &VfsPath) -> bool {
        self.fs.exists(path)
    }

    fn metadata(&self, path: &VfsPath) -> io::Result<std::fs::Metadata> {
        self.fs.metadata(path)
    }

    fn read_dir(&self, path: &VfsPath) -> io::Result<Vec<VfsPath>> {
        self.fs.read_dir(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use nova_core::{Position, Range};

    use crate::fs::LocalFs;

    #[test]
    fn vfs_emits_document_change_events() {
        let vfs = Vfs::new(LocalFs::new());
        let path = VfsPath::uri("file:///tmp/Main.java");
        let id = vfs.open_document(path.clone(), "hello world".to_string(), 1);

        let change = ContentChange::replace(
            Range::new(Position::new(0, 6), Position::new(0, 11)),
            "nova",
        );
        let evt = vfs.apply_document_changes(&path, 2, &[change]).unwrap();

        match evt {
            ChangeEvent::DocumentChanged {
                file_id,
                path: evt_path,
                version,
                edits,
            } => {
                assert_eq!(file_id, id);
                assert_eq!(evt_path, path);
                assert_eq!(version, 2);
                assert_eq!(edits.len(), 1);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn vfs_rename_path_preserves_id() {
        let vfs = Vfs::new(LocalFs::new());
        let dir = tempfile::tempdir().unwrap();
        let from = VfsPath::local(dir.path().join("a.java"));
        let to = VfsPath::local(dir.path().join("b.java"));

        let id = vfs.file_id(from.clone());
        let moved = vfs.rename_path(&from, to.clone());
        assert_eq!(id, moved);
        assert_eq!(vfs.get_id(&from), None);
        assert_eq!(vfs.get_id(&to), Some(id));
        assert_eq!(vfs.path_for_id(id), Some(to));
    }
}
