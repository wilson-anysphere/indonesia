use crate::error::CacheError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// A stable SHA-256 fingerprint stored as a lowercase hex string.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Fingerprint(String);

impl Fingerprint {
    /// Compute the SHA-256 fingerprint of an arbitrary byte slice.
    pub fn from_bytes(bytes: impl AsRef<[u8]>) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(bytes.as_ref());
        Self(hex::encode(hasher.finalize()))
    }

    /// Compute the SHA-256 fingerprint of a file's contents.
    ///
    /// This uses a streaming implementation to avoid reading large cache files
    /// (e.g. `.idx` indexes) into memory all at once.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, CacheError> {
        let mut file = std::fs::File::open(path)?;
        let mut hasher = Sha256::new();
        let mut buf = [0_u8; 64 * 1024];
        loop {
            let read = file.read(&mut buf)?;
            if read == 0 {
                break;
            }
            hasher.update(&buf[..read]);
        }
        Ok(Self(hex::encode(hasher.finalize())))
    }

    /// Compute a fast fingerprint based on file metadata (size + mtime).
    ///
    /// This avoids hashing full file contents and is intended for quick
    /// warm-start cache validation.
    pub fn from_file_metadata(path: impl AsRef<Path>) -> Result<Self, CacheError> {
        let meta = std::fs::metadata(path)?;
        let len = meta.len();
        let modified_nanos = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or(0);

        let mut bytes = Vec::with_capacity(8 + 16);
        bytes.extend_from_slice(&len.to_le_bytes());
        bytes.extend_from_slice(&modified_nanos.to_le_bytes());
        Ok(Self::from_bytes(bytes))
    }

    /// Create a fingerprint intended to identify a project directory.
    ///
    /// For sharing caches across machines/CI, we prefer a stable identifier that
    /// survives different checkout locations:
    ///
    /// - if `NOVA_PROJECT_ID` is set, hash that value
    /// - else if a git `remote.origin.url` is available, hash that URL
    /// - else fall back to hashing the canonicalized root path
    pub fn for_project_root(project_root: impl AsRef<Path>) -> Result<Self, CacheError> {
        let canonical = std::fs::canonicalize(project_root)?;

        if let Some(id) = std::env::var_os("NOVA_PROJECT_ID") {
            let id = id.to_string_lossy();
            if !id.trim().is_empty() {
                return Ok(Self::from_bytes(id.as_bytes()));
            }
        }

        if let Some(origin) = git_origin_url(&canonical) {
            return Ok(Self::from_bytes(origin.as_bytes()));
        }

        Ok(Self::from_bytes(canonical.to_string_lossy().as_bytes()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A snapshot of the inputs used to validate persistent caches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectSnapshot {
    project_root: PathBuf,
    project_hash: Fingerprint,
    file_fingerprints: std::collections::BTreeMap<String, Fingerprint>,
}

impl ProjectSnapshot {
    /// Create a snapshot from an explicit set of files.
    pub fn new(project_root: impl AsRef<Path>, files: Vec<PathBuf>) -> Result<Self, CacheError> {
        Self::new_with_fingerprinter(project_root, files, |path| Fingerprint::from_file(path))
    }

    /// Create a snapshot using fast per-file fingerprints (metadata only).
    ///
    /// This is suitable for quickly checking if a persisted cache is likely up
    /// to date without reading the full contents of every file.
    pub fn new_fast(project_root: impl AsRef<Path>, files: Vec<PathBuf>) -> Result<Self, CacheError> {
        Self::new_with_fingerprinter(project_root, files, |path| {
            Fingerprint::from_file_metadata(path)
        })
    }

    fn new_with_fingerprinter<F>(
        project_root: impl AsRef<Path>,
        files: Vec<PathBuf>,
        fingerprinter: F,
    ) -> Result<Self, CacheError>
    where
        F: Fn(&Path) -> Result<Fingerprint, CacheError>,
    {
        let project_root = std::fs::canonicalize(project_root)?;
        let project_hash = Fingerprint::for_project_root(&project_root)?;

        let mut file_fingerprints = std::collections::BTreeMap::new();
        for file in files {
            let full = if file.is_absolute() {
                file
            } else {
                project_root.join(file)
            };
            let full = std::fs::canonicalize(&full)?;
            let relative = full
                .strip_prefix(&project_root)
                .map_err(|_| CacheError::PathNotUnderProjectRoot {
                    path: full.clone(),
                    project_root: project_root.clone(),
                })?;
            let relative = relative.to_string_lossy().replace('\\', "/");
            file_fingerprints.insert(relative, fingerprinter(&full)?);
        }

        Ok(Self {
            project_root,
            project_hash,
            file_fingerprints,
        })
    }

    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    pub fn project_hash(&self) -> &Fingerprint {
        &self.project_hash
    }

    pub fn file_fingerprints(&self) -> &std::collections::BTreeMap<String, Fingerprint> {
        &self.file_fingerprints
    }
}

fn git_origin_url(project_root: &Path) -> Option<String> {
    let config_path = project_root.join(".git").join("config");
    let config = std::fs::read_to_string(config_path).ok()?;

    let mut in_origin = false;
    for raw_line in config.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }

        if line.starts_with('[') && line.ends_with(']') {
            in_origin = line.contains("remote \"origin\"") || line.contains("remote 'origin'");
            continue;
        }

        if !in_origin {
            continue;
        }

        let mut parts = line.splitn(2, '=');
        let key = parts.next()?.trim();
        let value = parts.next()?.trim();
        if key == "url" && !value.is_empty() {
            return Some(format!("git:{value}"));
        }
    }

    None
}
