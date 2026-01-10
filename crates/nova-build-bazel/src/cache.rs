use crate::aquery::JavaCompileInfo;
use anyhow::Result;
use blake3::Hash;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildFileDigest {
    pub path: PathBuf,
    pub digest_hex: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheEntry {
    pub target: String,
    pub query_hash_hex: String,
    pub build_files: Vec<BuildFileDigest>,
    pub info: JavaCompileInfo,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BazelCache {
    entries: HashMap<String, CacheEntry>,
}

impl BazelCache {
    pub fn get(
        &self,
        target: &str,
        query_hash: Hash,
        build_file_digests: &[BuildFileDigest],
    ) -> Option<&CacheEntry> {
        let entry = self.entries.get(target)?;
        if entry.query_hash_hex != hash_to_hex(query_hash) {
            return None;
        }
        if entry.build_files != build_file_digests {
            return None;
        }
        Some(entry)
    }

    pub fn insert(&mut self, entry: CacheEntry) {
        self.entries.insert(entry.target.clone(), entry);
    }

    pub fn invalidate_changed_build_files(&mut self, changed: &[PathBuf]) {
        if changed.is_empty() {
            return;
        }
        self.entries.retain(|_, entry| {
            !entry
                .build_files
                .iter()
                .any(|bf| changed.iter().any(|c| c == &bf.path))
        });
    }

    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let data = match fs::read_to_string(path) {
            Ok(data) => data,
            Err(_) => return Ok(Self::default()),
        };
        Ok(serde_json::from_str(&data).unwrap_or_default())
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_string_pretty(self)?;
        let tmp_path = path.with_extension("json.tmp");
        fs::write(&tmp_path, data)?;
        // `rename` is best-effort atomic on Unix. If it fails because the
        // destination exists (Windows), fall back to remove+rename.
        match fs::rename(&tmp_path, path) {
            Ok(()) => {}
            Err(_) if path.exists() => {
                let _ = fs::remove_file(path);
                fs::rename(&tmp_path, path)?;
            }
            Err(err) => return Err(err.into()),
        }
        Ok(())
    }
}

pub fn digest_file(path: &Path) -> Result<BuildFileDigest> {
    let bytes = fs::read(path)?;
    let hash = blake3::hash(&bytes);
    Ok(BuildFileDigest {
        path: path.to_path_buf(),
        digest_hex: hash_to_hex(hash),
    })
}

fn hash_to_hex(hash: Hash) -> String {
    hash.to_hex().to_string()
}
