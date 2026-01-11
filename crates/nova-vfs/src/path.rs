use std::fmt;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

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
        let archive = archive.into();
        let entry = entry.into();
        match normalize_archive_entry(&entry) {
            Some(entry) => Self::Archive(ArchivePath::new(ArchiveKind::Jar, archive, entry)),
            None => Self::Uri(format_archive_uri_fallback(ArchiveKind::Jar, &archive, &entry)),
        }
    }

    pub fn jmod(archive: impl Into<PathBuf>, entry: impl Into<String>) -> Self {
        let archive = archive.into();
        let entry = entry.into();
        match normalize_archive_entry(&entry) {
            Some(entry) => Self::Archive(ArchivePath::new(ArchiveKind::Jmod, archive, entry)),
            None => Self::Uri(format_archive_uri_fallback(ArchiveKind::Jmod, &archive, &entry)),
        }
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
                return Self::Local(normalize_local_path(path.as_path()));
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

    #[cfg(feature = "lsp")]
    pub fn to_lsp_uri(&self) -> Option<lsp_types::Uri> {
        self.to_uri()?.parse().ok()
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

#[cfg(feature = "lsp")]
impl From<&lsp_types::Uri> for VfsPath {
    fn from(value: &lsp_types::Uri) -> Self {
        VfsPath::uri(value.to_string())
    }
}

#[cfg(feature = "lsp")]
impl From<lsp_types::Uri> for VfsPath {
    fn from(value: lsp_types::Uri) -> Self {
        VfsPath::uri(value.to_string())
    }
}

fn normalize_archive_entry(entry: &str) -> Option<String> {
    let entry = entry.trim_start_matches(['/', '\\']);
    let entry = if entry.contains('\\') {
        entry.replace('\\', "/")
    } else {
        entry.to_string()
    };
    if entry.contains("//") {
        return None;
    }
    if entry.split('/').any(|segment| segment == "..") {
        return None;
    }
    Some(entry)
}

fn format_archive_uri_fallback(kind: ArchiveKind, archive: &Path, entry: &str) -> String {
    let scheme = match kind {
        ArchiveKind::Jar => "jar",
        ArchiveKind::Jmod => "jmod",
    };
    if let Ok(abs) = AbsPathBuf::new(archive.to_path_buf()) {
        if let Ok(archive_uri) = path_to_file_uri(&abs) {
            return format!("{scheme}:{archive_uri}!/{entry}");
        }
    }
    format!("{scheme}:{}!/{entry}", archive.display())
}

fn normalize_local_path(path: &Path) -> PathBuf {
    let mut prefix: Option<OsString> = None;
    let mut has_root = false;
    let mut stack: Vec<OsString> = Vec::new();

    for component in path.components() {
        match component {
            Component::Prefix(prefix_component) => {
                prefix = Some(prefix_component.as_os_str().to_owned());
            }
            Component::RootDir => has_root = true,
            Component::CurDir => {}
            Component::ParentDir => {
                if let Some(last) = stack.last() {
                    if last != ".." {
                        stack.pop();
                        continue;
                    }
                }

                if !has_root {
                    stack.push(OsString::from(".."));
                }
            }
            Component::Normal(segment) => stack.push(segment.to_owned()),
        }
    }

    let mut out = PathBuf::new();
    match (prefix, has_root) {
        (Some(mut prefix), true) => {
            prefix.push(std::path::MAIN_SEPARATOR.to_string());
            out.push(prefix);
        }
        (Some(prefix), false) => out.push(prefix),
        (None, true) => out.push(std::path::MAIN_SEPARATOR.to_string()),
        (None, false) => {}
    }
    out.extend(stack);
    out
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
    let archive = file_uri_to_path(archive_uri).ok()?;
    let archive = normalize_local_path(archive.as_path());
    let entry = normalize_archive_entry(entry)?;
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

    #[test]
    fn archive_entries_normalize_backslashes() {
        let dir = tempfile::tempdir().unwrap();
        let archive_path = dir.path().join("lib.jar");
        let a = VfsPath::jar(archive_path.clone(), "com\\example\\A.java");
        let b = VfsPath::jar(archive_path, "com/example/A.java");
        assert_eq!(a, b);
    }

    #[test]
    fn file_uri_paths_are_logically_normalized() {
        #[cfg(not(windows))]
        let (uri, expected) = ("file:///a/b/../c.java", PathBuf::from("/a/c.java"));

        #[cfg(windows)]
        let (uri, expected) = ("file:///C:/a/b/../c.java", PathBuf::from(r"C:\a\c.java"));

        assert_eq!(VfsPath::uri(uri), VfsPath::Local(expected));
    }

    #[test]
    fn jar_uris_reject_entry_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let archive_path = dir.path().join("lib.jar");
        let abs = AbsPathBuf::new(archive_path).unwrap();
        let archive_uri = path_to_file_uri(&abs).unwrap();

        let uri = format!("jar:{archive_uri}!/../evil.class");
        assert!(matches!(VfsPath::uri(uri), VfsPath::Uri(_)));

        let uri = format!("jar:{archive_uri}!/a/../evil.class");
        assert!(matches!(VfsPath::uri(uri), VfsPath::Uri(_)));
    }

    #[cfg(feature = "lsp")]
    #[test]
    fn lsp_uri_roundtrips_for_local_paths() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Main.java");
        let vfs_path = VfsPath::local(path.clone());
        let uri = vfs_path.to_lsp_uri().expect("lsp uri");
        let round = VfsPath::from(&uri);
        assert_eq!(round, vfs_path);
    }
}
