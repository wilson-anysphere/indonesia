use std::path::PathBuf;

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

    #[error("path {path} is not under project root {project_root}")]
    PathNotUnderProjectRoot { path: PathBuf, project_root: PathBuf },

    #[error("incompatible cache schema version: expected {expected}, found {found}")]
    IncompatibleSchemaVersion { expected: u32, found: u32 },

    #[error("incompatible nova version: expected {expected}, found {found}")]
    IncompatibleNovaVersion { expected: String, found: String },
}

