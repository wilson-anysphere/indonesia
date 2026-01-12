use crate::{SymbolKey, SymbolRange};
use nova_cache::{atomic_write, deps_cache_dir, CacheConfig, CacheError, Fingerprint};
use nova_core::{Position, Range};
use serde::{Deserialize, Serialize};
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
/// `<cache_root>/deps/decompiled/<hash>/<safe-stem>.java`.
///
/// `safe-stem` is a SHA-256 hex digest of the document's `binary_name`. This ensures the store is
/// robust to Windows-invalid filename characters and reserved device names (e.g. `CON`, `NUL`),
/// while keeping the external key as `(content_hash, binary_name)`.
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

    /// Persist decompiled source text *and* decompiler symbol mappings.
    ///
    /// The decompiled text is written to `<safe-stem>.java` (same as [`Self::store_text`]).
    /// Mappings are written to a JSON sidecar file next to it:
    /// `<safe-stem>.meta.json`.
    pub fn store_document(
        &self,
        content_hash: &str,
        binary_name: &str,
        text: &str,
        mappings: &[SymbolRange],
    ) -> Result<(), CacheError> {
        self.store_text(content_hash, binary_name, text)?;

        let meta_path = self.meta_path_for(content_hash, binary_name)?;
        let stored = StoredDecompiledMappings::from_mappings(mappings);
        let bytes = serde_json::to_vec(&stored)?;
        atomic_write(&meta_path, &bytes)
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

        let Some(bytes) = read_cache_file_bytes(&path)? else {
            return Ok(None);
        };

        match String::from_utf8(bytes) {
            Ok(text) => Ok(Some(text)),
            Err(_) => {
                let _ = std::fs::remove_file(&path);
                Ok(None)
            }
        }
    }

    /// Load previously-persisted decompiled source text and symbol mappings for a canonical
    /// `(content_hash, binary_name)` identity.
    ///
    /// This returns `Ok(None)` when:
    /// - the stored text file is missing or invalid
    /// - the mapping sidecar file is missing or invalid
    pub fn load_document(
        &self,
        content_hash: &str,
        binary_name: &str,
    ) -> Result<Option<(String, Vec<SymbolRange>)>, CacheError> {
        let Some(text) = self.load_text(content_hash, binary_name)? else {
            return Ok(None);
        };

        let meta_path = self.meta_path_for(content_hash, binary_name)?;
        let Some(meta_bytes) = read_cache_file_bytes(&meta_path)? else {
            return Ok(None);
        };

        let stored: StoredDecompiledMappings = match serde_json::from_slice(&meta_bytes) {
            Ok(value) => value,
            Err(_) => {
                let _ = std::fs::remove_file(&meta_path);
                return Ok(None);
            }
        };

        Ok(Some((text, stored.into_mappings())))
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
        let safe_stem = safe_binary_name_stem(binary_name);
        Ok(self
            .root
            .join(content_hash)
            .join(format!("{safe_stem}.java")))
    }

    fn meta_path_for(&self, content_hash: &str, binary_name: &str) -> Result<PathBuf, CacheError> {
        validate_content_hash(content_hash)?;
        validate_binary_name(binary_name)?;
        let safe_stem = safe_binary_name_stem(binary_name);
        Ok(self
            .root
            .join(content_hash)
            .join(format!("{safe_stem}.meta.json")))
    }
}

fn safe_binary_name_stem(binary_name: &str) -> Fingerprint {
    // Hash the binary name to produce an on-disk filename component that:
    // - is deterministic for a given `binary_name`
    // - never contains Windows-invalid filename characters (`<>:"/\\|?*`)
    // - never collides with Windows reserved device names (`CON`, `PRN`, `NUL`, ...), since it's a
    //   64-character hex digest.
    Fingerprint::from_bytes(binary_name.as_bytes())
}

fn read_cache_file_bytes(path: &Path) -> Result<Option<Vec<u8>>, CacheError> {
    // Avoid following symlinks out of the cache directory.
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    if meta.file_type().is_symlink() || !meta.is_file() {
        let _ = std::fs::remove_file(path);
        return Ok(None);
    }

    // Cap reads to avoid pathological allocations if the cache is corrupted.
    const MAX_DOC_BYTES: u64 = nova_cache::BINCODE_PAYLOAD_LIMIT_BYTES as u64;
    if meta.len() > MAX_DOC_BYTES {
        let _ = std::fs::remove_file(path);
        return Ok(None);
    }

    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    if bytes.len() as u64 > MAX_DOC_BYTES {
        let _ = std::fs::remove_file(path);
        return Ok(None);
    }

    Ok(Some(bytes))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredDecompiledMappings {
    mappings: Vec<StoredSymbolRange>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredSymbolRange {
    symbol: SymbolKey,
    start_line: u32,
    start_character: u32,
    end_line: u32,
    end_character: u32,
}

impl StoredDecompiledMappings {
    fn from_mappings(mappings: &[SymbolRange]) -> Self {
        Self {
            mappings: mappings
                .iter()
                .map(|m| StoredSymbolRange {
                    symbol: m.symbol.clone(),
                    start_line: m.range.start.line,
                    start_character: m.range.start.character,
                    end_line: m.range.end.line,
                    end_character: m.range.end.character,
                })
                .collect(),
        }
    }

    fn into_mappings(self) -> Vec<SymbolRange> {
        self.mappings
            .into_iter()
            .map(|m| SymbolRange {
                symbol: m.symbol,
                range: Range::new(
                    Position::new(m.start_line, m.start_character),
                    Position::new(m.end_line, m.end_character),
                ),
            })
            .collect()
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
