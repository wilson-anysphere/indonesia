//! Virtual file system layer for Nova.
//!
//! The VFS is responsible for:
//! - Reading files from the OS file system.
//! - Providing in-memory overlays (editor buffers) that take precedence over disk.
//! - Serving synthesized virtual documents (e.g. `nova:///decompiled/...`) from a bounded in-memory store.
//! - Stable `FileId` allocation and reverse mapping for diagnostics/LSP.
//! - Representing file change events and a pluggable watcher interface.

mod archive;
mod archive_reader;
mod change;
mod document;
mod file_id;
mod fs;
mod open_documents;
mod overlay_fs;
mod path;
mod vfs;
mod virtual_documents;
mod virtual_documents_fs;
mod watch;

pub use archive::{ArchiveKind, ArchivePath, ArchiveReader, StubArchiveReader};
pub use archive_reader::NovaArchiveReader;
pub use change::{ChangeEvent, ChangeKind, FileChange, FileChangeKind};
pub use document::{ContentChange, Document, DocumentError};
pub use file_id::FileIdRegistry;
pub use fs::{FileSystem, LocalFs};
pub use nova_core::FileId;
pub use open_documents::OpenDocuments;
pub use overlay_fs::OverlayFs;
pub use path::VfsPath;
pub use vfs::Vfs;
pub use virtual_documents::VirtualDocumentStore;
pub use virtual_documents_fs::VirtualDocumentsFs;
pub use watch::{
    FileWatcher, ManualFileWatcher, ManualFileWatcherHandle, WatchEvent, WatchMessage, WatchMode,
};

/// Lexically normalizes a local filesystem path using the same rules as `VfsPath::local`.
///
/// This does not hit the filesystem and does not resolve symlinks.
pub fn normalize_local_path(path: &std::path::Path) -> std::path::PathBuf {
    crate::path::normalize_local_path(path)
}

#[cfg(feature = "watch-notify")]
pub use watch::{EventNormalizer, NotifyFileWatcher};

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::Path;

    #[test]
    fn normalize_local_path_matches_vfs_path_local() {
        let raw = Path::new("a/./b/../c");
        let normalized = normalize_local_path(raw);
        assert_eq!(VfsPath::local(raw), VfsPath::Local(normalized));
    }
}
