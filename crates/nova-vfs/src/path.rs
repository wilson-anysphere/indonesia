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
        Self::Archive(ArchivePath::new(
            ArchiveKind::Jar,
            archive.into(),
            normalize_archive_entry(entry.into()),
        ))
    }

    pub fn jmod(archive: impl Into<PathBuf>, entry: impl Into<String>) -> Self {
        Self::Archive(ArchivePath::new(
            ArchiveKind::Jmod,
            archive.into(),
            normalize_archive_entry(entry.into()),
        ))
    }

    pub fn uri(uri: impl Into<String>) -> Self {
        let uri = uri.into();
        if let Some(archive) = parse_archive_uri(&uri) {
            return Self::Archive(archive);
        }
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

    /// Convert this path into a URI string suitable for editor/LSP-facing APIs.
    ///
    /// - Local absolute paths use the `file:` scheme.
    /// - Archive paths use `jar:` / `jmod:` and embed the archive's `file:` URI.
    /// - `Uri` paths are returned as-is.
    pub fn to_uri(&self) -> Option<String> {
        match self {
            VfsPath::Local(_) => self.to_file_uri(),
            VfsPath::Archive(path) => {
                let abs = AbsPathBuf::new(path.archive.clone()).ok()?;
                let archive_uri = path_to_file_uri(&abs).ok()?;
                let scheme = match path.kind {
                    ArchiveKind::Jar => "jar",
                    ArchiveKind::Jmod => "jmod",
                };
                let entry = path.entry.strip_prefix('/').unwrap_or(&path.entry);
                Some(format!("{scheme}:{archive_uri}!/{entry}"))
            }
            VfsPath::Uri(uri) => Some(uri.clone()),
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

fn normalize_archive_entry(entry: String) -> String {
    entry
        .strip_prefix('/')
        .unwrap_or(entry.as_str())
        .to_string()
}

fn parse_archive_uri(uri: &str) -> Option<ArchivePath> {
    let (kind, rest) = if let Some(rest) = uri.strip_prefix("jar:") {
        (ArchiveKind::Jar, rest)
    } else if let Some(rest) = uri.strip_prefix("jmod:") {
        (ArchiveKind::Jmod, rest)
    } else {
        return None;
    };

    let (archive_uri, entry) = rest.split_once('!')?;
    let archive = file_uri_to_path(archive_uri).ok()?.into_path_buf();
    let entry = normalize_archive_entry(entry.to_string());
    Some(ArchivePath::new(kind, archive, entry))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jar_uri_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let archive_path = dir.path().join("lib.jar");
        let jar = VfsPath::jar(archive_path, "/com/example/Foo.class");

        let uri = jar.to_uri().expect("jar uri");
        let round = VfsPath::uri(uri);
        assert_eq!(round, jar);
    }

    #[test]
    fn jmod_uri_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let archive_path = dir.path().join("java.base.jmod");
        let jmod = VfsPath::jmod(archive_path, "classes/java/lang/String.class");

        let uri = jmod.to_uri().expect("jmod uri");
        let round = VfsPath::uri(uri);
        assert_eq!(round, jmod);
    }
}
