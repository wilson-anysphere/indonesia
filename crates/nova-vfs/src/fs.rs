use std::fs;
use std::io;
use std::path::Path;
use std::sync::Arc;

use crate::archive::ArchiveReader;
use crate::archive_reader::NovaArchiveReader;
use crate::path::VfsPath;

/// File system abstraction for Nova.
///
/// The trait is intentionally small so it can be implemented for different
/// backends (local FS, overlays, future archives, etc).
pub trait FileSystem: Send + Sync {
    /// Reads the file contents as raw bytes.
    ///
    /// Implementations may return `ErrorKind::InvalidData` for paths that cannot
    /// be represented as bytes (e.g. synthesized virtual documents).
    fn read_bytes(&self, path: &VfsPath) -> io::Result<Vec<u8>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("byte reads not supported ({path})"),
        ))
    }

    /// Reads the file contents as UTF-8 text.
    fn read_to_string(&self, path: &VfsPath) -> io::Result<String>;

    /// Returns whether a path exists.
    fn exists(&self, path: &VfsPath) -> bool;

    /// Returns basic metadata for a path.
    fn metadata(&self, path: &VfsPath) -> io::Result<fs::Metadata>;

    /// Lists directory entries. Implementations may return `ErrorKind::Unsupported`.
    fn read_dir(&self, path: &VfsPath) -> io::Result<Vec<VfsPath>>;
}

/// Local OS file system implementation.
///
/// By default this uses [`crate::NovaArchiveReader`] to resolve [`VfsPath::Archive`] reads.
#[derive(Debug, Clone)]
pub struct LocalFs {
    archive: Arc<dyn ArchiveReader>,
}

impl LocalFs {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_archive_reader(archive: Arc<dyn ArchiveReader>) -> Self {
        Self { archive }
    }

    fn read_dir_local(path: &Path) -> io::Result<Vec<VfsPath>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            out.push(VfsPath::Local(entry.path()));
        }
        Ok(out)
    }
}

impl Default for LocalFs {
    fn default() -> Self {
        Self {
            archive: Arc::new(NovaArchiveReader),
        }
    }
}

impl FileSystem for LocalFs {
    fn read_bytes(&self, path: &VfsPath) -> io::Result<Vec<u8>> {
        match path {
            VfsPath::Local(path) => fs::read(path),
            VfsPath::Archive(path) => self.archive.read_bytes(path),
            VfsPath::Decompiled { .. } | VfsPath::LegacyDecompiled { .. } => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("cannot read decompiled document: {path}"),
            )),
            VfsPath::Uri(uri) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("cannot read URI path: {uri}"),
            )),
        }
    }

    fn read_to_string(&self, path: &VfsPath) -> io::Result<String> {
        match path {
            VfsPath::Local(path) => fs::read_to_string(path),
            VfsPath::Archive(path) => self.archive.read_to_string(path),
            VfsPath::Decompiled { .. } | VfsPath::LegacyDecompiled { .. } => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("cannot read decompiled document: {path}"),
            )),
            VfsPath::Uri(uri) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("cannot read URI path: {uri}"),
            )),
        }
    }

    fn exists(&self, path: &VfsPath) -> bool {
        match path {
            VfsPath::Local(path) => path.exists(),
            VfsPath::Archive(path) => self.archive.exists(path),
            VfsPath::Decompiled { .. } | VfsPath::LegacyDecompiled { .. } => false,
            VfsPath::Uri(_) => false,
        }
    }

    fn metadata(&self, path: &VfsPath) -> io::Result<fs::Metadata> {
        match path {
            VfsPath::Local(path) => fs::metadata(path),
            VfsPath::Archive(path) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("archive metadata not implemented ({path})"),
            )),
            VfsPath::Decompiled { .. } | VfsPath::LegacyDecompiled { .. } => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("decompiled document metadata not supported ({path})"),
            )),
            VfsPath::Uri(uri) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("URI metadata not supported ({uri})"),
            )),
        }
    }

    fn read_dir(&self, path: &VfsPath) -> io::Result<Vec<VfsPath>> {
        match path {
            VfsPath::Local(path) => Self::read_dir_local(path),
            VfsPath::Archive(path) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("archive directory listing not implemented ({path})"),
            )),
            VfsPath::Decompiled { .. } | VfsPath::LegacyDecompiled { .. } => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("decompiled document directory listing not supported ({path})"),
            )),
            VfsPath::Uri(uri) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("URI directory listing not supported ({uri})"),
            )),
        }
    }
}
