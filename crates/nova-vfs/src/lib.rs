//! Virtual file system layer for Nova.
//!
//! The VFS is responsible for:
//! - Reading files from the OS file system.
//! - Providing in-memory overlays (editor buffers) that take precedence over disk.
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
mod watch;

pub use archive::{ArchiveKind, ArchivePath, ArchiveReader, StubArchiveReader};
pub use archive_reader::NovaArchiveReader;
pub use change::{ChangeEvent, ChangeKind, FileChange, FileChangeKind};
pub use document::{ContentChange, Document, DocumentError};
pub use file_id::FileIdRegistry;
pub use nova_core::FileId;
pub use fs::{FileSystem, LocalFs};
pub use open_documents::OpenDocuments;
pub use overlay_fs::OverlayFs;
pub use path::VfsPath;
pub use vfs::Vfs;
pub use watch::{FileWatcher, WatchEvent};
