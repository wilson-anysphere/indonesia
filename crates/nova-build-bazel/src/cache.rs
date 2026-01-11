use crate::aquery::JavaCompileInfo;
use anyhow::Result;
use blake3::Hash;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    io,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

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
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "path has no parent"))?;

        let (tmp_path, mut file) = open_unique_tmp_file(path, parent)?;
        if let Err(err) = file.write_all(data.as_bytes()) {
            drop(file);
            let _ = fs::remove_file(&tmp_path);
            return Err(err.into());
        }
        drop(file);

        // `rename` is best-effort atomic on Unix. If it fails because the
        // destination exists (Windows), fall back to remove+rename.
        const MAX_RENAME_ATTEMPTS: usize = 1024;
        let rename_result = (|| -> io::Result<()> {
            let mut attempts = 0usize;
            loop {
                match fs::rename(&tmp_path, path) {
                    Ok(()) => return Ok(()),
                    Err(err) if err.kind() == io::ErrorKind::AlreadyExists || path.exists() => {
                        let _ = fs::remove_file(path);

                        attempts += 1;
                        if attempts >= MAX_RENAME_ATTEMPTS {
                            return Err(err);
                        }

                        continue;
                    }
                    Err(err) => return Err(err),
                }
            }
        })();

        if let Err(err) = rename_result {
            let _ = fs::remove_file(&tmp_path);
            return Err(err.into());
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

fn open_unique_tmp_file(dest: &Path, parent: &Path) -> io::Result<(PathBuf, fs::File)> {
    let file_name = dest.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::Other, "destination path has no file name")
    })?;
    let pid = std::process::id();

    loop {
        let counter = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut tmp_name = file_name.to_os_string();
        tmp_name.push(format!(".tmp.{pid}.{counter}"));
        let tmp_path = parent.join(tmp_name);

        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
        {
            Ok(file) => return Ok((tmp_path, file)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
}
