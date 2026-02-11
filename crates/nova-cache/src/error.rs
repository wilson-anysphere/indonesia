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
    // Conservatively redact all double-quoted substrings. This keeps the error actionable (it
    // retains the overall structure + line/column info) without echoing potentially-sensitive
    // content embedded in strings.
    let mut out = String::with_capacity(message.len());
    let mut rest = message;
    while let Some(start) = rest.find('"') {
        // Include the opening quote.
        out.push_str(&rest[..start + 1]);
        rest = &rest[start + 1..];

        let mut end = None;
        let bytes = rest.as_bytes();
        for (idx, &b) in bytes.iter().enumerate() {
            if b != b'"' {
                continue;
            }

            // Treat quotes preceded by an odd number of backslashes as escaped.
            let mut backslashes = 0usize;
            let mut k = idx;
            while k > 0 && bytes[k - 1] == b'\\' {
                backslashes += 1;
                k -= 1;
            }
            if backslashes % 2 == 0 {
                end = Some(idx);
                break;
            }
        }

        let Some(end) = end else {
            // Unterminated quote: redact the remainder and stop.
            out.push_str("<redacted>");
            rest = "";
            break;
        };
        out.push_str("<redacted>\"");
        rest = &rest[end + 1..];
    }
    out.push_str(rest);

    // `serde` wraps unknown fields/variants in backticks:
    // `unknown field `secret`, expected ...`
    //
    // Redact only the first backticked segment so we keep the expected value list actionable.
    if let Some(start) = out.find('`') {
        let after_start = &out[start.saturating_add(1)..];
        let end = if let Some(end_rel) = after_start.find("`, expected") {
            Some(start.saturating_add(1).saturating_add(end_rel))
        } else if let Some(end_rel) = after_start.find('`') {
            Some(start.saturating_add(1).saturating_add(end_rel))
        } else {
            None
        };
        if let Some(end) = end {
            if start + 1 <= end && end <= out.len() {
                out.replace_range(start + 1..end, "<redacted>");
            }
        }
    }

    out
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
}
