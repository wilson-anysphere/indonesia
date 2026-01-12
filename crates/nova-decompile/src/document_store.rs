use nova_cache::{atomic_write, deps_cache_dir, CacheConfig, CacheError};
use std::io;
use std::path::{Component, Path, PathBuf};

/// Persistent, content-addressed store for canonical ADR0006 decompiled virtual documents.
///
/// Canonical decompiled URIs have the form:
/// `nova:///decompiled/<content-hash>/<binary-name>.java`.
///
/// This store persists the *rendered* decompiled text keyed by the same `(content_hash,
/// binary_name)` segments so clients (e.g. `nova-lsp`) can warm-start decompiled buffers without
/// recomputing them.
///
/// ## On-disk layout
///
/// By default (`from_env`), documents are stored under Nova's global dependency cache:
/// `<cache_root>/deps/decompiled/<hash>/<binary-name>.java`.
#[derive(Debug, Clone)]
pub struct DecompiledDocumentStore {
    root: PathBuf,
}

impl DecompiledDocumentStore {
    /// Construct the store using Nova's default cache location.
    ///
    /// This uses [`CacheConfig::from_env`] (respecting `NOVA_CACHE_DIR`) and stores documents
    /// under the global deps cache (`.../deps/decompiled`).
    pub fn from_env() -> Result<Self, CacheError> {
        let config = CacheConfig::from_env();
        let deps_root = deps_cache_dir(&config)?;
        Ok(Self::new(deps_root.join("decompiled")))
    }

    /// Construct a store rooted at `root`.
    ///
    /// This is primarily intended for tests; callers should usually prefer [`Self::from_env`].
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Persist decompiled source text for a canonical `(content_hash, binary_name)` identity.
    ///
    /// Writes are atomic and safe under concurrent writers.
    pub fn store_text(
        &self,
        content_hash: &str,
        binary_name: &str,
        text: &str,
    ) -> Result<(), CacheError> {
        let path = self.path_for(content_hash, binary_name)?;
        atomic_write(&path, text.as_bytes())
    }

    /// Load previously-persisted decompiled source text for a canonical `(content_hash,
    /// binary_name)` identity.
    ///
    /// This is best-effort: missing files or obvious corruption (non-file, symlink, invalid
    /// UTF-8) return `Ok(None)`.
    pub fn load_text(
        &self,
        content_hash: &str,
        binary_name: &str,
    ) -> Result<Option<String>, CacheError> {
        let path = self.path_for(content_hash, binary_name)?;

        // Avoid following symlinks out of the cache directory.
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        if meta.file_type().is_symlink() || !meta.is_file() {
            let _ = std::fs::remove_file(&path);
            return Ok(None);
        }

        // Cap reads to avoid pathological allocations if the cache is corrupted.
        const MAX_DOC_BYTES: u64 = nova_cache::BINCODE_PAYLOAD_LIMIT_BYTES as u64;
        if meta.len() > MAX_DOC_BYTES {
            let _ = std::fs::remove_file(&path);
            return Ok(None);
        }

        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        if bytes.len() as u64 > MAX_DOC_BYTES {
            let _ = std::fs::remove_file(&path);
            return Ok(None);
        }

        match String::from_utf8(bytes) {
            Ok(text) => Ok(Some(text)),
            Err(_) => {
                let _ = std::fs::remove_file(&path);
                Ok(None)
            }
        }
    }

    /// Returns whether the decompiled document exists on disk.
    ///
    /// Invalid `(content_hash, binary_name)` inputs return `false`.
    pub fn exists(&self, content_hash: &str, binary_name: &str) -> bool {
        let Ok(path) = self.path_for(content_hash, binary_name) else {
            return false;
        };
        path.exists()
    }

    /// Convenience wrapper around [`Self::store_text`] that takes a canonical `nova:///...` URI.
    pub fn store_uri(&self, uri: &str, text: &str) -> Result<(), CacheError> {
        let parsed = crate::parse_decompiled_uri(uri)
            .ok_or_else(|| io::Error::other("invalid decompiled virtual document URI"))?;
        self.store_text(&parsed.content_hash, &parsed.binary_name, text)
    }

    /// Convenience wrapper around [`Self::load_text`] that takes a canonical `nova:///...` URI.
    pub fn load_uri(&self, uri: &str) -> Result<Option<String>, CacheError> {
        let parsed = crate::parse_decompiled_uri(uri)
            .ok_or_else(|| io::Error::other("invalid decompiled virtual document URI"))?;
        self.load_text(&parsed.content_hash, &parsed.binary_name)
    }

    fn path_for(&self, content_hash: &str, binary_name: &str) -> Result<PathBuf, CacheError> {
        validate_content_hash(content_hash)?;
        validate_binary_name(binary_name)?;
        Ok(self
            .root
            .join(content_hash)
            .join(format!("{binary_name}.java")))
    }
}

fn validate_content_hash(content_hash: &str) -> Result<(), CacheError> {
    if content_hash.len() != 64 {
        return Err(io::Error::other("invalid decompiled content hash length").into());
    }

    // Fingerprints are stored/printed as lowercase hex (`nova_cache::Fingerprint`).
    if !content_hash
        .bytes()
        .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
    {
        return Err(io::Error::other(
            "invalid decompiled content hash (expected 64 lowercase hex characters)",
        )
        .into());
    }

    Ok(())
}

fn validate_binary_name(binary_name: &str) -> Result<(), CacheError> {
    if binary_name.is_empty() {
        return Err(io::Error::other("invalid decompiled binary name (empty)").into());
    }
    if binary_name.contains('/') || binary_name.contains('\\') {
        return Err(io::Error::other(
            "invalid decompiled binary name (contains path separators)",
        )
        .into());
    }

    // Reject drive prefixes / absolute paths / dot segments.
    let mut components = Path::new(binary_name).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) => Ok(()),
        _ => Err(io::Error::other("invalid decompiled binary name").into()),
    }
}

