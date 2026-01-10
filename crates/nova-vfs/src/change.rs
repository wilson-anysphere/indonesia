use nova_core::{FileId, TextEdit};

use crate::path::VfsPath;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileChangeKind {
    Created,
    Modified,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FileChange {
    pub path: VfsPath,
    pub kind: FileChangeKind,
}

/// High-level change kinds produced by the VFS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChangeKind {
    FileSystem(FileChangeKind),
    Document,
}

/// A change event emitted by watchers/overlays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeEvent {
    /// A file changed on disk.
    FileSystem(FileChange),
    /// An open document changed via LSP edits.
    DocumentChanged {
        file_id: FileId,
        path: VfsPath,
        version: i32,
        edits: Vec<TextEdit>,
    },
}

impl ChangeEvent {
    pub fn kind(&self) -> ChangeKind {
        match self {
            ChangeEvent::FileSystem(change) => ChangeKind::FileSystem(change.kind),
            ChangeEvent::DocumentChanged { .. } => ChangeKind::Document,
        }
    }
}
