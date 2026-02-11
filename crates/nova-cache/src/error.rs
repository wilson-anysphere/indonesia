use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, CacheError>;

/// Errors produced by cache management and persistence.
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("failed to determine home directory for default cache path")]
    MissingHomeDir,

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {message}")]
    Json { message: String },

    #[error("bincode error: {0}")]
    Bincode(#[from] bincode::Error),

    #[error("storage error: {0}")]
    Storage(#[from] nova_storage::StorageError),

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

impl From<serde_json::Error> for CacheError {
    fn from(err: serde_json::Error) -> Self {
        // `serde_json::Error` display strings can include user-provided scalar values (e.g.
        // `invalid type: string "..."`). Cache metadata can contain user paths and other sensitive
        // inputs; avoid echoing string values in error messages.
        let message = sanitize_json_error_message(&err.to_string());
        Self::Json { message }
    }
}

fn sanitize_json_error_message(message: &str) -> String {
    nova_core::sanitize_json_error_message(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_error_json_does_not_echo_string_values() {
        let secret_suffix = "nova-cache-super-secret-token";
        let secret = format!("prefix\"{secret_suffix}");
        let err = serde_json::from_value::<bool>(serde_json::json!(secret))
            .expect_err("expected type error");

        let cache_err = CacheError::from(err);
        let message = cache_err.to_string();
        assert!(
            !message.contains(secret_suffix),
            "expected CacheError json message to omit string values: {message}"
        );
        assert!(
            message.contains("<redacted>"),
            "expected CacheError json message to include redaction marker: {message}"
        );
    }

    #[test]
    fn cache_error_json_does_not_echo_backticked_values() {
        #[derive(Debug, serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct OnlyFoo {
            #[allow(dead_code)]
            foo: u32,
        }

        let secret_suffix = "nova-cache-backticked-secret";
        let secret = format!("prefix`, expected {secret_suffix}");
        let json = format!(r#"{{"{secret}": 1}}"#);
        let err = serde_json::from_str::<OnlyFoo>(&json).expect_err("expected unknown field error");
        let raw_message = err.to_string();
        assert!(
            raw_message.contains(secret_suffix),
            "expected raw serde_json unknown-field error string to include the backticked value so this test catches leaks: {raw_message}"
        );

        let cache_err = CacheError::from(err);
        let message = cache_err.to_string();
        assert!(
            !message.contains(secret_suffix),
            "expected CacheError json message to omit backticked values: {message}"
        );
        assert!(
            message.contains("<redacted>"),
            "expected CacheError json message to include redaction marker: {message}"
        );
    }
}
