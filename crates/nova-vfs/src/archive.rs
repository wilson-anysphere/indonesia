use std::fmt;
use std::io;
use std::path::PathBuf;

/// Supported archive container types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArchiveKind {
    Jar,
    Jmod,
}

/// A path to a file inside an archive.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ArchivePath {
    pub kind: ArchiveKind,
    pub archive: PathBuf,
    pub entry: String,
}

impl ArchivePath {
    pub fn new(kind: ArchiveKind, archive: PathBuf, entry: String) -> Self {
        Self {
            kind,
            archive,
            entry,
        }
    }
}

impl fmt::Display for ArchivePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.kind {
            ArchiveKind::Jar => "jar",
            ArchiveKind::Jmod => "jmod",
        };
        write!(
            f,
            "{kind}:{}!{}",
            self.archive.display(),
            self.entry
        )
    }
}

/// Pluggable interface for reading files from archives (`.jar`/`.jmod`).
///
/// This is an abstraction hook; the default implementation in this crate is a
/// stub that returns `ErrorKind::Unsupported`.
pub trait ArchiveReader: Send + Sync {
    fn read_to_string(&self, path: &ArchivePath) -> io::Result<String>;

    fn exists(&self, path: &ArchivePath) -> bool;
}

/// Stub `ArchiveReader` used until real archive support is implemented.
#[derive(Debug, Default)]
pub struct StubArchiveReader;

impl ArchiveReader for StubArchiveReader {
    fn read_to_string(&self, path: &ArchivePath) -> io::Result<String> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("archive reading not implemented ({path})"),
        ))
    }

    fn exists(&self, _path: &ArchivePath) -> bool {
        false
    }
}

