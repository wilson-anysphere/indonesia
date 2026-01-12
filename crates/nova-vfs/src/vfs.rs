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
    ///
    /// If `from` is open in the overlay, the in-memory document is updated so that reads through
    /// `to` continue to see overlay contents instead of falling back to disk.
    ///
    /// If `to` is already interned, the [`FileIdRegistry`] typically keeps the destination
    /// `FileId` and drops the source mapping (treating this as a delete + modify).
    ///
    /// However, when renaming an open document onto an interned destination path that is not open
    /// in the overlay, we preserve the source `FileId` and orphan the destination id. This keeps
    /// `FileId`s stable for open documents even if the destination was previously observed by the
    /// watcher.
    pub fn rename_path(&self, from: &VfsPath, to: VfsPath) -> FileId {
        let to_clone = to.clone();

        // Capture the pre-rename ids so we can keep open-document tracking consistent if the
        // rename collapses `from` into an existing destination id.
        let id_from = self.get_id(from);
        let id_to = self.get_id(&to_clone);

        // Update the overlay first so reads through `to` still see in-memory document contents.
        let from_open = self.fs.is_open(from);
        let to_open = self.fs.is_open(&to_clone);
        if from_open {
            self.fs.rename(from, to_clone.clone());
        }

        let id = {
            let mut ids = self.ids.lock().expect("file id registry mutex poisoned");
            if from_open && !to_open {
                match (id_from, id_to) {
                    (Some(id_from), Some(id_to)) if id_from != id_to => {
                        ids.rename_path_displacing_destination(from, to)
                    }
                    _ => ids.rename_path(from, to),
                }
            } else {
                ids.rename_path(from, to)
            }
        };

        // If the rename changed the file id for an open document, make sure `OpenDocuments`
        // doesn't retain a now-unreachable id.
        if from_open {
            if let Some(id_from) = id_from {
                if id_from != id {
                    self.open_docs.close(id_from);
                }
                if let Some(id_to) = id_to {
                    if id_to != id {
                        self.open_docs.close(id_to);
                    }
                }
                if self.fs.is_open(&to_clone) {
                    self.open_docs.open(id);
                }
            }
        }

        id
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

    #[cfg(feature = "lsp")]
    pub fn open_document_lsp(&self, uri: lsp_types::Uri, text: String, version: i32) -> FileId {
        self.open_document(VfsPath::from(uri), text, version)
    }

    pub fn close_document(&self, path: &VfsPath) {
        if let Some(id) = self.get_id(path) {
            self.open_docs.close(id);
        }
        self.fs.close(path);
    }

    #[cfg(feature = "lsp")]
    pub fn close_document_lsp(&self, uri: &lsp_types::Uri) {
        self.close_document(&VfsPath::from(uri))
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

    #[cfg(feature = "lsp")]
    pub fn apply_document_changes_lsp(
        &self,
        uri: &lsp_types::Uri,
        new_version: i32,
        changes: &[lsp_types::TextDocumentContentChangeEvent],
    ) -> Result<ChangeEvent, DocumentError> {
        let path = VfsPath::from(uri);
        let changes: Vec<ContentChange> =
            changes.iter().cloned().map(ContentChange::from).collect();
        self.apply_document_changes(&path, new_version, &changes)
    }
}

impl<F: FileSystem> FileSystem for Vfs<F> {
    fn read_bytes(&self, path: &VfsPath) -> io::Result<Vec<u8>> {
        self.fs.read_bytes(path)
    }

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

    use nova_core::{AbsPathBuf, Position, Range};

    use crate::fs::LocalFs;

    #[test]
    fn vfs_emits_document_change_events() {
        let vfs = Vfs::new(LocalFs::new());
        let tmp = tempfile::tempdir().unwrap();
        let abs = AbsPathBuf::new(tmp.path().join("Main.java")).unwrap();
        let uri = nova_core::path_to_file_uri(&abs).unwrap();
        let path = VfsPath::uri(uri);
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

    #[test]
    fn vfs_rename_path_moves_open_overlay_document() {
        let vfs = Vfs::new(LocalFs::new());
        let dir = tempfile::tempdir().unwrap();
        let from = VfsPath::local(dir.path().join("a.java"));
        let to = VfsPath::local(dir.path().join("b.java"));

        let id = vfs.open_document(from.clone(), "hello".to_string(), 1);
        assert!(vfs.open_documents().is_open(id));
        assert!(vfs.overlay().is_open(&from));

        let moved = vfs.rename_path(&from, to.clone());
        assert_eq!(moved, id);

        assert!(!vfs.overlay().is_open(&from));
        assert!(vfs.overlay().is_open(&to));
        assert_eq!(vfs.read_to_string(&to).unwrap(), "hello");
        assert_eq!(vfs.get_id(&to), Some(id));
        assert_eq!(vfs.path_for_id(id), Some(to));
        assert!(vfs.open_documents().is_open(id));
    }

    #[test]
    fn vfs_rename_path_to_existing_path_preserves_source_id_for_open_document() {
        let vfs = Vfs::new(LocalFs::new());
        let dir = tempfile::tempdir().unwrap();
        let from = VfsPath::local(dir.path().join("a.java"));
        let to = VfsPath::local(dir.path().join("b.java"));

        let to_id = vfs.file_id(to.clone());
        let from_id = vfs.open_document(from.clone(), "hello".to_string(), 1);
        assert_ne!(to_id, from_id);
        assert!(vfs.open_documents().is_open(from_id));

        let moved = vfs.rename_path(&from, to.clone());
        assert_eq!(moved, from_id);
        assert_eq!(vfs.get_id(&to), Some(from_id));
        assert_eq!(vfs.path_for_id(from_id), Some(to.clone()));
        assert_eq!(vfs.path_for_id(to_id), None);

        assert!(!vfs.overlay().is_open(&from));
        assert!(vfs.overlay().is_open(&to));
        assert_eq!(vfs.read_to_string(&to).unwrap(), "hello");
        assert!(vfs.open_documents().is_open(from_id));
        assert!(!vfs.open_documents().is_open(to_id));
    }

    #[test]
    fn vfs_rename_path_to_open_destination_keeps_destination_id() {
        let vfs = Vfs::new(LocalFs::new());
        let dir = tempfile::tempdir().unwrap();
        let from = VfsPath::local(dir.path().join("a.java"));
        let to = VfsPath::local(dir.path().join("b.java"));

        let to_id = vfs.open_document(to.clone(), "dst".to_string(), 1);
        let from_id = vfs.open_document(from.clone(), "src".to_string(), 1);
        assert_ne!(to_id, from_id);

        let moved = vfs.rename_path(&from, to.clone());
        assert_eq!(moved, to_id);

        assert!(!vfs.overlay().is_open(&from));
        assert!(vfs.overlay().is_open(&to));
        assert_eq!(vfs.read_to_string(&to).unwrap(), "dst");

        assert!(vfs.open_documents().is_open(to_id));
        assert!(!vfs.open_documents().is_open(from_id));
    }

    #[cfg(feature = "lsp")]
    #[test]
    fn vfs_accepts_lsp_document_changes() {
        let vfs = Vfs::new(LocalFs::new());
        let dir = tempfile::tempdir().unwrap();
        let path = VfsPath::local(dir.path().join("Main.java"));
        let uri = path.to_lsp_uri().unwrap();

        let id = vfs.open_document_lsp(uri.clone(), "hello world".to_string(), 1);

        let change = lsp_types::TextDocumentContentChangeEvent {
            range: Some(lsp_types::Range {
                start: lsp_types::Position {
                    line: 0,
                    character: 6,
                },
                end: lsp_types::Position {
                    line: 0,
                    character: 11,
                },
            }),
            range_length: None,
            text: "nova".to_string(),
        };

        let evt = vfs.apply_document_changes_lsp(&uri, 2, &[change]).unwrap();
        assert_eq!(
            vfs.read_to_string(&VfsPath::from(&uri)).unwrap(),
            "hello nova"
        );

        match evt {
            ChangeEvent::DocumentChanged {
                file_id,
                version,
                edits,
                ..
            } => {
                assert_eq!(file_id, id);
                assert_eq!(version, 2);
                assert_eq!(edits.len(), 1);
                assert_eq!(u32::from(edits[0].range.start()), 6);
                assert_eq!(u32::from(edits[0].range.end()), 11);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }
}
