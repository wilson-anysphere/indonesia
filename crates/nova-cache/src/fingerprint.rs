use crate::error::CacheError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::UNIX_EPOCH;

/// A stable SHA-256 fingerprint stored as a lowercase hex string.
#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[archive(check_bytes)]
#[serde(transparent)]
pub struct Fingerprint(String);

impl Fingerprint {
    /// Compute the SHA-256 fingerprint of an arbitrary byte slice.
    pub fn from_bytes(bytes: impl AsRef<[u8]>) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(bytes.as_ref());
        Self(hex::encode(hasher.finalize()))
    }

    /// Compute the SHA-256 fingerprint of bytes read from `reader`.
    pub fn from_reader(mut reader: impl Read) -> Result<Self, CacheError> {
        let mut hasher = Sha256::new();
        let mut buf = [0_u8; 64 * 1024];
        loop {
            let read = reader.read(&mut buf)?;
            if read == 0 {
                break;
            }
            hasher.update(&buf[..read]);
        }
        Ok(Self(hex::encode(hasher.finalize())))
    }

    /// Compute the SHA-256 fingerprint of a file's contents.
    ///
    /// This uses a streaming implementation to avoid reading large cache files
    /// (e.g. `.idx` indexes) into memory all at once.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, CacheError> {
        let file = std::fs::File::open(path)?;
        Self::from_reader(file)
    }

    /// Compute a fast fingerprint based on file metadata (size + mtime).
    ///
    /// This avoids hashing full file contents and is intended for quick
    /// warm-start cache validation.
    pub fn from_file_metadata(path: impl AsRef<Path>) -> Result<Self, CacheError> {
        let path = path.as_ref();
        let meta = std::fs::metadata(path)?;
        let len = meta.len();
        let modified_nanos: u128 = match meta.modified() {
            Ok(time) => match time.duration_since(UNIX_EPOCH) {
                Ok(dur) => dur.as_nanos(),
                Err(err) => {
                    static REPORTED_MTIME_BEFORE_EPOCH: OnceLock<()> = OnceLock::new();
                    if REPORTED_MTIME_BEFORE_EPOCH.set(()).is_ok() {
                        tracing::debug!(
                            target = "nova.cache",
                            path = %path.display(),
                            error = ?err,
                            "file mtime is before UNIX_EPOCH; using 0 for metadata fingerprint"
                        );
                    }
                    0
                }
            },
            Err(err) => {
                static REPORTED_MTIME_ERROR: OnceLock<()> = OnceLock::new();
                if REPORTED_MTIME_ERROR.set(()).is_ok() {
                    tracing::debug!(
                        target = "nova.cache",
                        path = %path.display(),
                        error = %err,
                        "failed to read file mtime; using 0 for metadata fingerprint"
                    );
                }
                0
            }
        };

        let mut bytes = Vec::with_capacity(8 + 16);
        bytes.extend_from_slice(&len.to_le_bytes());
        bytes.extend_from_slice(&modified_nanos.to_le_bytes());
        Ok(Self::from_bytes(bytes))
    }

    /// Create a fingerprint intended to identify a project directory.
    ///
    /// For sharing caches across machines/CI, we prefer a stable identifier that
    /// survives different checkout locations.
    ///
    /// Fallback order:
    /// 1. `NOVA_PROJECT_ID` environment variable (if set and non-empty)
    /// 2. git `remote "origin"` `url = ...` (walking up from `project_root` looking for `.git`)
    /// 3. canonicalized `project_root` path
    pub fn for_project_root(project_root: impl AsRef<Path>) -> Result<Self, CacheError> {
        if let Some(id) = std::env::var_os("NOVA_PROJECT_ID") {
            let id = id.to_string_lossy();
            if !id.trim().is_empty() {
                return Ok(Self::from_bytes(id.as_bytes()));
            }
        }

        let canonical = std::fs::canonicalize(project_root)?;

        if let Some(origin) = git_origin_url(&canonical) {
            return Ok(Self::from_bytes(origin.as_bytes()));
        }

        Ok(Self::from_bytes(canonical.to_string_lossy().as_bytes()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl ArchivedFingerprint {
    pub fn as_str(&self) -> &str {
        self.0.as_str()
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
    pub fn new_fast(
        project_root: impl AsRef<Path>,
        files: Vec<PathBuf>,
    ) -> Result<Self, CacheError> {
        Self::new_with_fingerprinter(project_root, files, |path| {
            Fingerprint::from_file_metadata(path)
        })
    }

    /// Construct a snapshot from already-computed fingerprints.
    ///
    /// This is primarily intended for incremental indexing flows where the
    /// caller already has the content fingerprints for the files it read (e.g.
    /// the files it re-indexed) and wants to persist updated cache metadata
    /// without re-reading every file in the project.
    pub fn from_parts(
        project_root: PathBuf,
        project_hash: Fingerprint,
        file_fingerprints: std::collections::BTreeMap<String, Fingerprint>,
    ) -> Self {
        Self {
            project_root,
            project_hash,
            file_fingerprints,
        }
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
            let relative = full.strip_prefix(&project_root).map_err(|_| {
                CacheError::PathNotUnderProjectRoot {
                    path: full.clone(),
                    project_root: project_root.clone(),
                }
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

    /// Build a snapshot from already computed fingerprints.
    ///
    /// This is intended for workflows like index compaction where the persisted
    /// cache metadata already records the fingerprints associated with the
    /// current index state and recomputing them would be wasted work.
    pub fn from_fingerprints(
        project_root: impl AsRef<Path>,
        project_hash: Fingerprint,
        file_fingerprints: std::collections::BTreeMap<String, Fingerprint>,
    ) -> Result<Self, CacheError> {
        let project_root = std::fs::canonicalize(project_root)?;
        Ok(Self {
            project_root,
            project_hash,
            file_fingerprints,
        })
    }
}

fn git_origin_url(project_root: &Path) -> Option<String> {
    // Walk upwards looking for `.git` so that nested project roots (e.g. when Nova is
    // opened in a subdirectory) still share a stable cache key with the repo root.
    //
    // We support:
    // - `.git/config` when `.git` is a directory.
    // - `.git` as a file that contains `gitdir: <path>` (worktrees, submodules).
    for repo_root in project_root.ancestors() {
        let dot_git = repo_root.join(".git");
        let metadata = match std::fs::metadata(&dot_git) {
            Ok(metadata) => metadata,
            Err(err) => {
                // Most ancestors won't be git repos; only log unexpected filesystem errors.
                if err.kind() != std::io::ErrorKind::NotFound {
                    tracing::debug!(
                        target = "nova.cache",
                        dot_git = %dot_git.display(),
                        error = %err,
                        "failed to stat .git while resolving git origin"
                    );
                }
                continue;
            }
        };

        let mut config_paths = Vec::new();
        if metadata.is_dir() {
            config_paths.push(dot_git.join("config"));
        } else if metadata.is_file() {
            let gitdir = resolve_gitdir(&dot_git, repo_root)?;
            config_paths.push(gitdir.join("config"));

            // In linked worktrees, the remotes often live in the "common" git
            // directory. Best-effort read it as well.
            if let Some(commondir) = resolve_commondir(&gitdir) {
                config_paths.push(commondir.join("config"));
            }
        } else {
            continue;
        }

        for config_path in config_paths {
            let config = match std::fs::read_to_string(&config_path) {
                Ok(config) => config,
                Err(err) => {
                    if err.kind() != std::io::ErrorKind::NotFound {
                        tracing::debug!(
                            target = "nova.cache",
                            config_path = %config_path.display(),
                            error = %err,
                            "failed to read git config while resolving origin"
                        );
                    }
                    continue;
                }
            };
            if let Some(origin) = parse_git_origin_from_config(&config) {
                return Some(origin);
            }
        }

        // `.git` was found but no origin URL was discovered; stop searching since
        // this is the repository boundary.
        return None;
    }

    None
}

fn parse_git_origin_from_config(config: &str) -> Option<String> {
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

fn resolve_gitdir(dot_git_file: &Path, repo_root: &Path) -> Option<PathBuf> {
    let contents = match std::fs::read_to_string(dot_git_file) {
        Ok(contents) => contents,
        Err(err) => {
            // Workspaces may not be git repos; only log unexpected filesystem errors.
            if err.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!(
                    target = "nova.cache",
                    dot_git_file = %dot_git_file.display(),
                    error = %err,
                    "failed to read .git file while resolving gitdir"
                );
            }
            return None;
        }
    };
    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }

        let path = line.strip_prefix("gitdir:")?.trim();
        if path.is_empty() {
            return None;
        }

        let path = PathBuf::from(path);
        return Some(if path.is_absolute() {
            path
        } else {
            repo_root.join(path)
        });
    }

    None
}

fn resolve_commondir(gitdir: &Path) -> Option<PathBuf> {
    let commondir_path = gitdir.join("commondir");
    let contents = match std::fs::read_to_string(&commondir_path) {
        Ok(contents) => contents,
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!(
                    target = "nova.cache",
                    commondir_path = %commondir_path.display(),
                    error = %err,
                    "failed to read git commondir marker"
                );
            }
            return None;
        }
    };
    let line = contents.lines().next()?.trim();
    if line.is_empty() {
        return None;
    }

    let path = PathBuf::from(line);
    Some(if path.is_absolute() {
        path
    } else {
        gitdir.join(path)
    })
}
