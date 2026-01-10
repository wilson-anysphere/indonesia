use std::fmt;
use std::path::{Path, PathBuf};

use nova_core::{file_uri_to_path, path_to_file_uri, AbsPathBuf};

use crate::archive::{ArchiveKind, ArchivePath};

/// A path that can be resolved by the VFS.
///
/// Today this supports local file system paths and archive paths. In the future
/// additional schemes (e.g. remote URIs) can be added.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum VfsPath {
    /// A file on the local OS file system.
    Local(PathBuf),
    /// A file inside an archive such as a `.jar` or `.jmod`.
    Archive(ArchivePath),
    /// A generic URI string that an external implementation can resolve.
    Uri(String),
}

impl VfsPath {
    pub fn local(path: impl Into<PathBuf>) -> Self {
        Self::Local(path.into())
    }

    pub fn jar(archive: impl Into<PathBuf>, entry: impl Into<String>) -> Self {
        Self::Archive(ArchivePath::new(ArchiveKind::Jar, archive.into(), entry.into()))
    }

    pub fn jmod(archive: impl Into<PathBuf>, entry: impl Into<String>) -> Self {
        Self::Archive(ArchivePath::new(ArchiveKind::Jmod, archive.into(), entry.into()))
    }

    pub fn uri(uri: impl Into<String>) -> Self {
        let uri = uri.into();
        // Treat `file:` URIs as local paths so that LSP buffers and disk paths
        // map to the same `VfsPath`/`FileId`.
        if uri.starts_with("file:") {
            if let Ok(path) = file_uri_to_path(&uri) {
                return Self::Local(path.into_path_buf());
            }
        }
        Self::Uri(uri)
    }

    pub fn as_local_path(&self) -> Option<&Path> {
        match self {
            VfsPath::Local(path) => Some(path.as_path()),
            _ => None,
        }
    }

    /// Convert this path into a `file://` URI, if it represents an absolute local path.
    pub fn to_file_uri(&self) -> Option<String> {
        match self {
            VfsPath::Local(path) => {
                let abs = AbsPathBuf::new(path.clone()).ok()?;
                path_to_file_uri(&abs).ok()
            }
            _ => None,
        }
    }
}

impl From<PathBuf> for VfsPath {
    fn from(value: PathBuf) -> Self {
        VfsPath::Local(value)
    }
}

impl From<&Path> for VfsPath {
    fn from(value: &Path) -> Self {
        VfsPath::Local(value.to_path_buf())
    }
}

impl fmt::Display for VfsPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VfsPath::Local(path) => write!(f, "{}", path.display()),
            VfsPath::Archive(archive) => write!(f, "{archive}"),
            VfsPath::Uri(uri) => write!(f, "{uri}"),
        }
    }
}
