use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, CacheError>;

/// Errors produced by cache management and persistence.
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("failed to determine home directory for default cache path")]
    MissingHomeDir,

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("bincode error: {0}")]
    Bincode(#[from] bincode::Error),

    #[error("walkdir error: {0}")]
    WalkDir(#[from] walkdir::Error),

    #[error("path {path} is not under project root {project_root}")]
    PathNotUnderProjectRoot {
        path: PathBuf,
        project_root: PathBuf,
    },

    #[error("path {path} is not under cache root {cache_root}")]
    PathNotUnderCacheRoot { path: PathBuf, cache_root: PathBuf },

    #[error("incompatible cache schema version: expected {expected}, found {found}")]
    IncompatibleSchemaVersion { expected: u32, found: u32 },

    #[error("incompatible nova version: expected {expected}, found {found}")]
    IncompatibleNovaVersion { expected: String, found: String },

    #[error("invalid archive path: {path:?}")]
    InvalidArchivePath { path: PathBuf },

    #[error("unsupported archive entry type for {path:?}")]
    UnsupportedArchiveEntryType { path: PathBuf },

    #[error("archive is missing required entry {path}")]
    MissingArchiveEntry { path: &'static str },

    #[error("cache package checksum mismatch for {path}: expected {expected}, found {found}")]
    ChecksumMismatch {
        path: String,
        expected: String,
        found: String,
    },

    #[error("cache package is missing checksum entry for {path}")]
    MissingChecksum { path: String },

    #[error("http fetch failed: {message}")]
    Http { message: String },

    #[error("unsupported fetch URL {url}")]
    UnsupportedFetchUrl { url: String },

    #[error("cache package project hash mismatch: expected {expected}, found {found}")]
    IncompatibleProjectHash { expected: String, found: String },

    #[cfg(feature = "s3")]
    #[error("s3 fetch failed: {message}")]
    S3 { message: String },
}
