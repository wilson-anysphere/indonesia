use crate::error::CacheError;
use crate::fingerprint::Fingerprint;
use std::path::{Path, PathBuf};

/// Configuration for selecting the on-disk cache root.
#[derive(Clone, Debug, Default)]
pub struct CacheConfig {
    /// Override the global cache directory (the project hash is still appended).
    pub cache_root_override: Option<PathBuf>,
}

impl CacheConfig {
    pub fn from_env() -> Self {
        Self {
            cache_root_override: std::env::var_os("NOVA_CACHE_DIR").map(PathBuf::from),
        }
    }
}

#[derive(Clone, Debug)]
pub struct CacheDir {
    project_root: PathBuf,
    project_hash: Fingerprint,
    root: PathBuf,
}

impl CacheDir {
    pub fn new(project_root: impl AsRef<Path>, config: CacheConfig) -> Result<Self, CacheError> {
        let project_root = std::fs::canonicalize(project_root)?;
        let project_hash = Fingerprint::for_project_root(&project_root)?;

        let base = match config.cache_root_override {
            Some(root) => root,
            None => default_cache_root()?,
        };

        let root = base.join(project_hash.as_str());

        std::fs::create_dir_all(root.join("indexes"))?;
        std::fs::create_dir_all(root.join("queries"))?;
        std::fs::create_dir_all(root.join("ast"))?;

        Ok(Self {
            project_root,
            project_hash,
            root,
        })
    }

    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    pub fn project_hash(&self) -> &Fingerprint {
        &self.project_hash
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn indexes_dir(&self) -> PathBuf {
        self.root.join("indexes")
    }

    pub fn queries_dir(&self) -> PathBuf {
        self.root.join("queries")
    }

    pub fn ast_dir(&self) -> PathBuf {
        self.root.join("ast")
    }

    pub fn metadata_path(&self) -> PathBuf {
        self.root.join("metadata.json")
    }
}

fn default_cache_root() -> Result<PathBuf, CacheError> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or(CacheError::MissingHomeDir)?;

    Ok(home.join(".nova").join("cache"))
}

