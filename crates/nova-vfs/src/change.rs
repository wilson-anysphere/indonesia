use nova_core::{FileId, TextEdit};

use crate::path::VfsPath;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileChangeKind {
    Created,
    Modified,
    Deleted,
    Moved,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FileChange {
    Created { path: VfsPath },
    Modified { path: VfsPath },
    Deleted { path: VfsPath },
    Moved { from: VfsPath, to: VfsPath },
}

impl FileChange {
    pub fn kind(&self) -> FileChangeKind {
        match self {
            FileChange::Created { .. } => FileChangeKind::Created,
            FileChange::Modified { .. } => FileChangeKind::Modified,
            FileChange::Deleted { .. } => FileChangeKind::Deleted,
            FileChange::Moved { .. } => FileChangeKind::Moved,
        }
    }

    /// Returns every path touched by this change.
    ///
    /// - For create/modify/delete this is just the path.
    /// - For moves this includes both `from` and `to`.
    pub fn paths(&self) -> impl Iterator<Item = &VfsPath> {
        let (first, second) = match self {
            FileChange::Created { path }
            | FileChange::Modified { path }
            | FileChange::Deleted { path } => (path, None),
            FileChange::Moved { from, to } => (from, Some(to)),
        };

        std::iter::once(first).chain(second)
    }
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
            ChangeEvent::FileSystem(change) => ChangeKind::FileSystem(change.kind()),
            ChangeEvent::DocumentChanged { .. } => ChangeKind::Document,
        }
    }
}
