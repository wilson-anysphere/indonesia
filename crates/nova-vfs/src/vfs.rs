use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use nova_core::FileId;

use crate::change::ChangeEvent;
use crate::document::{ContentChange, DocumentError};
use crate::file_id::FileIdRegistry;
use crate::fs::FileSystem;
use crate::open_documents::OpenDocuments;
use crate::overlay_fs::OverlayFs;
use crate::path::VfsPath;
use crate::virtual_documents::VirtualDocumentStore;
use crate::virtual_documents_fs::VirtualDocumentsFs;

/// High-level VFS facade combining:
/// - a base `FileSystem` (usually `LocalFs`)
/// - an in-memory overlay for open documents (`OverlayFs`)
/// - a bounded in-memory store for virtual documents (`VirtualDocumentStore`)
/// - stable `FileId` allocation (`FileIdRegistry`)
#[derive(Debug, Clone)]
pub struct Vfs<F: FileSystem> {
    fs: OverlayFs<VirtualDocumentsFs<F>>,
    ids: Arc<Mutex<FileIdRegistry>>,
    open_docs: Arc<OpenDocuments>,
    virtual_documents: VirtualDocumentStore,
}

impl<F: FileSystem> Vfs<F> {
    /// Default memory budget for cached virtual documents (decompiled sources, etc).
    ///
    /// The budget is enforced via LRU eviction based on `text.len()` bytes.
    pub const DEFAULT_VIRTUAL_DOCUMENT_BUDGET_BYTES: usize = 64 * 1024 * 1024;

    pub fn new(base: F) -> Self {
        Self::new_with_virtual_document_budget(base, Self::DEFAULT_VIRTUAL_DOCUMENT_BUDGET_BYTES)
    }

    /// Constructs a new `Vfs` with an explicit budget for cached virtual documents.
    ///
    /// This is mainly intended for tests and memory-constrained environments.
    pub fn new_with_virtual_document_budget(base: F, virtual_document_max_bytes: usize) -> Self {
        let virtual_documents = VirtualDocumentStore::new(virtual_document_max_bytes);
        let base = VirtualDocumentsFs::new(base, virtual_documents.clone());
        Self {
            fs: OverlayFs::new(base),
            ids: Arc::new(Mutex::new(FileIdRegistry::new())),
            open_docs: Arc::new(OpenDocuments::default()),
            virtual_documents,
        }
    }

    pub fn overlay(&self) -> &OverlayFs<VirtualDocumentsFs<F>> {
        &self.fs
    }

    /// Best-effort estimate of the total number of UTF-8 bytes held in memory by this VFS.
    ///
    /// This includes:
    /// - editor overlay documents (open buffers)
    /// - cached virtual documents (decompiled sources, etc.)
    ///
    /// Values are tracked using `text.len()` (not `String` capacity) and are intended for coarse
    /// memory accounting (`nova-memory`) and diagnostics.
    pub fn estimated_bytes(&self) -> usize {
        self.fs
            .estimated_bytes()
            .saturating_add(self.virtual_documents.estimated_bytes())
    }

    /// Returns a shared handle to the set of open document ids.
    pub fn open_documents(&self) -> Arc<OpenDocuments> {
        self.open_docs.clone()
    }

    #[track_caller]
    fn lock_ids(&self) -> MutexGuard<'_, FileIdRegistry> {
        match self.ids.lock() {
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

    /// Returns the stable id for `path`, allocating one if needed.
    pub fn file_id(&self, path: VfsPath) -> FileId {
        let mut ids = self.lock_ids();
        ids.file_id(path)
    }

    /// Returns the id for `path` if it has been interned.
    pub fn get_id(&self, path: &VfsPath) -> Option<FileId> {
        let ids = self.lock_ids();
        ids.get_id(path)
    }

    /// Reverse lookup for an interned file id.
    pub fn path_for_id(&self, id: FileId) -> Option<VfsPath> {
        let ids = self.lock_ids();
        ids.get_path(id).cloned()
    }

    /// Rename (or move) a path, preserving the existing `FileId` when possible.
    ///
    /// If `from` is open in the overlay, the in-memory document is updated so that reads through
    /// `to` continue to see overlay contents instead of falling back to disk.
    ///
    /// If `to` is already interned, the [`FileIdRegistry`] keeps the destination `FileId` and
    /// drops the source mapping (treating this as a delete + modify).
    pub fn rename_path(&self, from: &VfsPath, to: VfsPath) -> FileId {
        let to_clone = to.clone();

        // Capture the pre-rename ids so we can keep open-document tracking consistent if the
        // rename collapses `from` into an existing destination id.
        let id_from = self.get_id(from);

        // Update the overlay first so reads through `to` still see in-memory document contents.
        let from_open = self.fs.is_open(from);
        if from_open {
            self.fs.rename(from, to_clone.clone());
        }

        let id = {
            let mut ids = self.lock_ids();
            ids.rename_path(from, to)
        };

        // If the rename changed the file id for an open document, make sure `OpenDocuments`
        // doesn't retain a now-unreachable id.
        if from_open {
            if let Some(id_from) = id_from {
                if id_from != id {
                    self.open_docs.close(id_from);
                }
            }
            if self.fs.is_open(&to_clone) {
                self.open_docs.open(id);
            }
        }

        id
    }

    /// Returns all currently-tracked file ids (sorted).
    pub fn all_file_ids(&self) -> Vec<FileId> {
        let ids = self.lock_ids();
        ids.all_file_ids()
    }

    /// Returns all currently-tracked file ids (unsorted).
    ///
    /// This is intended for callers that will impose their own ordering and want to avoid the
    /// extra `FileId` sort.
    pub fn all_file_ids_unsorted(&self) -> Vec<FileId> {
        let ids = self.lock_ids();
        ids.all_file_ids_unsorted()
    }

    pub fn for_each_local_path(&self, f: impl FnMut(&Path)) {
        let ids = self.lock_ids();
        ids.for_each_local_path(f);
    }

    /// Opens an in-memory overlay document and returns its `FileId`.
    pub fn open_document_arc(&self, path: VfsPath, text: Arc<String>, version: i32) -> FileId {
        let id = self.file_id(path.clone());
        self.fs.open_arc(path, text, version);
        self.open_docs.open(id);
        id
    }

    /// Opens an in-memory overlay document and returns its `FileId`.
    pub fn open_document(&self, path: VfsPath, text: String, version: i32) -> FileId {
        self.open_document_arc(path, Arc::new(text), version)
    }

    /// Stores a virtual document (e.g. a decompiled source file) for later reads through the VFS.
    ///
    /// Only [`VfsPath::Decompiled`] and [`VfsPath::LegacyDecompiled`] are stored; other paths are
    /// ignored.
    pub fn store_virtual_document(&self, path: VfsPath, text: String) {
        self.virtual_documents.insert_text(path, text);
    }

    #[cfg(feature = "lsp")]
    pub fn open_document_lsp(&self, uri: lsp_types::Uri, text: String, version: i32) -> FileId {
        self.open_document(VfsPath::from(uri), text, version)
    }

    pub fn open_document_text_arc(&self, path: &VfsPath) -> Option<Arc<String>> {
        self.fs.document_text_arc(path)
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

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
    fn vfs_rename_path_to_existing_path_keeps_destination_id_for_open_document() {
        let vfs = Vfs::new(LocalFs::new());
        let dir = tempfile::tempdir().unwrap();
        let from = VfsPath::local(dir.path().join("a.java"));
        let to = VfsPath::local(dir.path().join("b.java"));

        let to_id = vfs.file_id(to.clone());
        let from_id = vfs.open_document(from.clone(), "hello".to_string(), 1);
        assert_ne!(to_id, from_id);
        assert!(vfs.open_documents().is_open(from_id));

        let moved = vfs.rename_path(&from, to.clone());
        assert_eq!(moved, to_id);
        assert_eq!(vfs.get_id(&to), Some(to_id));
        assert_eq!(vfs.path_for_id(to_id), Some(to.clone()));
        assert_eq!(vfs.path_for_id(from_id), None);

        assert!(!vfs.overlay().is_open(&from));
        assert!(vfs.overlay().is_open(&to));
        assert_eq!(vfs.read_to_string(&to).unwrap(), "hello");
        assert!(!vfs.open_documents().is_open(from_id));
        assert!(vfs.open_documents().is_open(to_id));
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

    const HASH_64: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn decompiled_virtual_documents_are_readable_without_overlay() {
        let vfs = Vfs::new(LocalFs::new());
        let path = VfsPath::decompiled(HASH_64, "com.example.Foo");

        vfs.store_virtual_document(path.clone(), "class Foo {}".to_string());
        assert_eq!(vfs.read_to_string(&path).unwrap(), "class Foo {}");
    }

    #[test]
    fn decompiled_virtual_documents_fall_back_to_base_fs_when_not_cached() {
        #[derive(Clone, Debug)]
        struct MockFs {
            path: VfsPath,
            text: String,
            reads: Arc<AtomicUsize>,
        }

        impl FileSystem for MockFs {
            fn read_to_string(&self, path: &VfsPath) -> io::Result<String> {
                if path == &self.path {
                    self.reads.fetch_add(1, Ordering::SeqCst);
                    return Ok(self.text.clone());
                }
                Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("missing ({path})"),
                ))
            }

            fn exists(&self, path: &VfsPath) -> bool {
                path == &self.path
            }

            fn metadata(&self, path: &VfsPath) -> io::Result<std::fs::Metadata> {
                Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("metadata not supported ({path})"),
                ))
            }

            fn read_dir(&self, path: &VfsPath) -> io::Result<Vec<VfsPath>> {
                Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("read_dir not supported ({path})"),
                ))
            }
        }

        let path = VfsPath::decompiled(HASH_64, "com.example.FromBase");
        let reads = Arc::new(AtomicUsize::new(0));
        let base = MockFs {
            path: path.clone(),
            text: "class FromBase {}".to_string(),
            reads: reads.clone(),
        };
        let vfs = Vfs::new(base);

        // The decompiled document should be readable without explicit
        // `store_virtual_document` calls when the base filesystem supports it.
        assert_eq!(vfs.read_to_string(&path).unwrap(), "class FromBase {}");
        assert!(vfs.virtual_documents.contains(&path));
        assert_eq!(reads.load(Ordering::SeqCst), 1);

        // Follow-up reads should hit the in-memory cache and avoid re-reading
        // from the base filesystem.
        assert_eq!(vfs.read_to_string(&path).unwrap(), "class FromBase {}");
        assert_eq!(reads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn decompiled_virtual_document_exists_falls_back_to_base_fs_without_caching() {
        #[derive(Clone, Debug)]
        struct ExistsFs {
            path: VfsPath,
            exists_calls: Arc<AtomicUsize>,
        }

        impl FileSystem for ExistsFs {
            fn read_to_string(&self, path: &VfsPath) -> io::Result<String> {
                panic!("unexpected base.read_to_string({path})");
            }

            fn exists(&self, path: &VfsPath) -> bool {
                if path == &self.path {
                    self.exists_calls.fetch_add(1, Ordering::SeqCst);
                    return true;
                }
                false
            }

            fn metadata(&self, path: &VfsPath) -> io::Result<std::fs::Metadata> {
                Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("metadata not supported ({path})"),
                ))
            }

            fn read_dir(&self, path: &VfsPath) -> io::Result<Vec<VfsPath>> {
                Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("read_dir not supported ({path})"),
                ))
            }
        }

        let path = VfsPath::decompiled(HASH_64, "com.example.ExistsFromBase");
        let exists_calls = Arc::new(AtomicUsize::new(0));
        let base = ExistsFs {
            path: path.clone(),
            exists_calls: exists_calls.clone(),
        };
        let vfs = Vfs::new(base);

        assert!(vfs.exists(&path));
        assert_eq!(exists_calls.load(Ordering::SeqCst), 1);
        assert!(
            !vfs.virtual_documents.contains(&path),
            "exists() should not populate the virtual document cache"
        );
    }

    #[test]
    fn decompiled_virtual_documents_read_bytes_fall_back_to_base_fs_when_not_cached() {
        #[derive(Clone, Debug)]
        struct ByteFs {
            path: VfsPath,
            bytes: Vec<u8>,
            reads: Arc<AtomicUsize>,
        }

        impl FileSystem for ByteFs {
            fn read_bytes(&self, path: &VfsPath) -> io::Result<Vec<u8>> {
                if path == &self.path {
                    self.reads.fetch_add(1, Ordering::SeqCst);
                    return Ok(self.bytes.clone());
                }
                Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("missing ({path})"),
                ))
            }

            fn read_to_string(&self, path: &VfsPath) -> io::Result<String> {
                panic!("unexpected base.read_to_string({path})");
            }

            fn exists(&self, path: &VfsPath) -> bool {
                path == &self.path
            }

            fn metadata(&self, path: &VfsPath) -> io::Result<std::fs::Metadata> {
                Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("metadata not supported ({path})"),
                ))
            }

            fn read_dir(&self, path: &VfsPath) -> io::Result<Vec<VfsPath>> {
                Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("read_dir not supported ({path})"),
                ))
            }
        }

        let path = VfsPath::decompiled(HASH_64, "com.example.BytesFromBase");
        let reads = Arc::new(AtomicUsize::new(0));
        let base = ByteFs {
            path: path.clone(),
            bytes: b"class BytesFromBase {}".to_vec(),
            reads: reads.clone(),
        };
        let vfs = Vfs::new(base);

        assert_eq!(
            vfs.read_bytes(&path).unwrap(),
            b"class BytesFromBase {}".to_vec()
        );
        assert!(vfs.virtual_documents.contains(&path));
        assert_eq!(reads.load(Ordering::SeqCst), 1);

        // Follow-up reads should hit the cache instead of consulting the base FS again.
        assert_eq!(vfs.read_to_string(&path).unwrap(), "class BytesFromBase {}");
        assert_eq!(
            vfs.read_bytes(&path).unwrap(),
            b"class BytesFromBase {}".to_vec()
        );
        assert_eq!(reads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn missing_decompiled_virtual_documents_return_not_found() {
        let vfs = Vfs::new(LocalFs::new());
        let path = VfsPath::decompiled(HASH_64, "com.example.Missing");

        let err = vfs
            .read_to_string(&path)
            .expect_err("expected read to fail");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn overlay_precedence_over_virtual_document_store() {
        let vfs = Vfs::new(LocalFs::new());
        let path = VfsPath::decompiled(HASH_64, "com.example.Foo");

        vfs.store_virtual_document(path.clone(), "store".to_string());
        vfs.open_document(path.clone(), "overlay".to_string(), 1);

        assert_eq!(vfs.read_to_string(&path).unwrap(), "overlay");
    }

    #[test]
    fn overlay_precedence_over_virtual_document_store_for_read_bytes() {
        let vfs = Vfs::new(LocalFs::new());
        let path = VfsPath::decompiled(HASH_64, "com.example.Foo");

        vfs.store_virtual_document(path.clone(), "store".to_string());
        vfs.open_document(path.clone(), "overlay".to_string(), 1);

        assert_eq!(vfs.read_bytes(&path).unwrap(), b"overlay".to_vec());
    }

    #[test]
    fn vfs_can_be_constructed_with_zero_virtual_document_budget() {
        let vfs = Vfs::new_with_virtual_document_budget(LocalFs::new(), 0);
        let path = VfsPath::decompiled(HASH_64, "com.example.Foo");

        vfs.store_virtual_document(path.clone(), "class Foo {}".to_string());
        let err = vfs
            .read_to_string(&path)
            .expect_err("expected budget=0 to store nothing");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn vfs_exists_reports_virtual_document_presence() {
        let vfs = Vfs::new(LocalFs::new());
        let path = VfsPath::decompiled(HASH_64, "com.example.Foo");

        assert!(
            !vfs.exists(&path),
            "virtual doc should be absent by default"
        );

        vfs.store_virtual_document(path.clone(), "class Foo {}".to_string());
        assert!(vfs.exists(&path), "virtual doc should exist after storing");
    }

    #[test]
    fn vfs_read_bytes_serves_virtual_document_text() {
        let vfs = Vfs::new(LocalFs::new());
        let path = VfsPath::decompiled(HASH_64, "com.example.Bytes");
        let text = "class Bytes {}".to_string();

        vfs.store_virtual_document(path.clone(), text.clone());
        assert_eq!(vfs.read_bytes(&path).unwrap(), text.into_bytes());
    }

    #[test]
    fn vfs_virtual_document_metadata_is_unsupported() {
        let vfs = Vfs::new(LocalFs::new());
        let path = VfsPath::decompiled(HASH_64, "com.example.Meta");

        vfs.store_virtual_document(path.clone(), "class Meta {}".to_string());

        let err = vfs
            .metadata(&path)
            .expect_err("expected metadata to be unsupported");
        assert_eq!(err.kind(), std::io::ErrorKind::Unsupported);
    }

    #[test]
    fn vfs_virtual_document_read_dir_is_unsupported() {
        let vfs = Vfs::new(LocalFs::new());
        let path = VfsPath::decompiled(HASH_64, "com.example.Dir");

        vfs.store_virtual_document(path.clone(), "class Dir {}".to_string());

        let err = vfs
            .read_dir(&path)
            .expect_err("expected read_dir to be unsupported");
        assert_eq!(err.kind(), std::io::ErrorKind::Unsupported);
    }

    #[test]
    fn vfs_virtual_document_metadata_is_unsupported_even_if_missing() {
        let vfs = Vfs::new(LocalFs::new());
        let path = VfsPath::decompiled(HASH_64, "com.example.MissingMeta");

        let err = vfs
            .metadata(&path)
            .expect_err("expected metadata to be unsupported");
        assert_eq!(err.kind(), std::io::ErrorKind::Unsupported);
    }

    #[test]
    fn legacy_decompiled_virtual_documents_are_readable() {
        let vfs = Vfs::new(LocalFs::new());
        let path = VfsPath::legacy_decompiled("com/example/Foo");

        vfs.store_virtual_document(path.clone(), "class Foo {}".to_string());
        assert_eq!(vfs.read_to_string(&path).unwrap(), "class Foo {}");
        assert!(vfs.exists(&path));
    }

    #[test]
    fn missing_virtual_document_read_bytes_returns_not_found() {
        let vfs = Vfs::new(LocalFs::new());
        let path = VfsPath::decompiled(HASH_64, "com.example.MissingBytes");

        let err = vfs
            .read_bytes(&path)
            .expect_err("expected byte read to fail");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }
}
