use crate::aquery::JavaCompileInfo;
use anyhow::Result;
use blake3::Hash;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs, io,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CompileInfoProvider {
    /// Compile information extracted from `bazel aquery`.
    #[default]
    Aquery,
    /// Compile information extracted from BSP `buildTarget/javacOptions`.
    Bsp,
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDigest {
    pub path: PathBuf,
    pub digest_hex: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheEntry {
    pub target: String,
    /// Digest of the Bazel query/aquery expressions (and output format) used to compute `info`.
    pub expr_version_hex: String,
    /// Digests of all files that influence `info`.
    ///
    /// This includes workspace-level configuration (`WORKSPACE`, `MODULE.bazel`, `.bazelrc`, ...)
    /// and the BUILD files for packages in the target's transitive dependency closure.
    pub files: Vec<FileDigest>,
    #[serde(default)]
    pub provider: CompileInfoProvider,
    pub info: JavaCompileInfo,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BazelCache {
    entries: HashMap<String, CacheEntry>,
}

impl BazelCache {
    fn cache_key(target: &str, provider: CompileInfoProvider) -> String {
        format!(
            "{}:{target}",
            match provider {
                CompileInfoProvider::Aquery => "aquery",
                CompileInfoProvider::Bsp => "bsp",
            }
        )
    }

    pub fn get(
        &self,
        target: &str,
        expr_version_hex: &str,
        provider: CompileInfoProvider,
    ) -> Option<&CacheEntry> {
        let key = Self::cache_key(target, provider);
        let entry = self.entries.get(&key).or_else(|| {
            // Backwards compatibility: older cache files keyed directly by the label.
            if provider == CompileInfoProvider::Aquery {
                self.entries.get(target)
            } else {
                None
            }
        })?;
        if entry.expr_version_hex != expr_version_hex {
            return None;
        }

        // Recompute digests to validate the entry. This avoids invoking Bazel when cached entries
        // are still valid.
        let current_digests = digest_files(&entry.files).ok()?;
        if entry.files != current_digests {
            return None;
        }
        if entry.provider != provider {
            return None;
        }
        Some(entry)
    }

    pub fn insert(&mut self, entry: CacheEntry) {
        let key = Self::cache_key(&entry.target, entry.provider);
        if key != entry.target {
            // Remove any legacy key for the same label.
            self.entries.remove(&entry.target);
        }
        self.entries.insert(key, entry);
    }

    pub fn invalidate_changed_files(&mut self, changed: &[PathBuf]) {
        if changed.is_empty() {
            return;
        }
        self.entries.retain(|_, entry| {
            !entry
                .files
                .iter()
                .any(|f| changed.iter().any(|c| c == &f.path))
        });
    }

    pub fn invalidate_changed_build_files(&mut self, changed: &[PathBuf]) {
        self.invalidate_changed_files(changed);
    }

    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let data = match fs::read_to_string(path) {
            Ok(data) => data,
            Err(_) => return Ok(Self::default()),
        };
        let mut cache: Self = serde_json::from_str(&data).unwrap_or_default();
        cache.migrate_keys();
        Ok(cache)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("path has no parent"))?;
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        fs::create_dir_all(parent)?;

        let data = serde_json::to_string_pretty(self)?;

        let (tmp_path, mut file) = open_unique_tmp_file(path, parent)?;
        if let Err(err) = file
            .write_all(data.as_bytes())
            .and_then(|()| file.sync_all())
        {
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
                    Err(err)
                        if cfg!(windows)
                            && (err.kind() == io::ErrorKind::AlreadyExists || path.exists()) =>
                    {
                        match fs::remove_file(path) {
                            Ok(()) => {}
                            Err(remove_err) if remove_err.kind() == io::ErrorKind::NotFound => {}
                            Err(remove_err) => return Err(remove_err),
                        }

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

        #[cfg(unix)]
        {
            let _ = fs::File::open(parent).and_then(|dir| dir.sync_all());
        }

        Ok(())
    }

    fn migrate_keys(&mut self) {
        let entries = std::mem::take(&mut self.entries);
        let mut migrated = HashMap::with_capacity(entries.len());
        for (_key, entry) in entries {
            let key = Self::cache_key(&entry.target, entry.provider);
            migrated.entry(key).or_insert(entry);
        }
        self.entries = migrated;
    }
}

pub fn digest_file(path: &Path) -> Result<FileDigest> {
    let bytes = fs::read(path)?;
    let hash = blake3::hash(&bytes);
    Ok(FileDigest {
        path: path.to_path_buf(),
        digest_hex: hash_to_hex(hash),
    })
}

pub fn digest_file_or_absent(path: &Path) -> Result<FileDigest> {
    match fs::read(path) {
        Ok(bytes) => Ok(FileDigest {
            path: path.to_path_buf(),
            digest_hex: hash_to_hex(blake3::hash(&bytes)),
        }),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(FileDigest {
            path: path.to_path_buf(),
            digest_hex: "absent".to_string(),
        }),
        Err(err) => Err(err.into()),
    }
}

fn hash_to_hex(hash: Hash) -> String {
    hash.to_hex().to_string()
}

fn digest_files(files: &[FileDigest]) -> Result<Vec<FileDigest>> {
    let mut out = Vec::with_capacity(files.len());
    for file in files {
        out.push(digest_file_or_absent(&file.path)?);
    }
    Ok(out)
}

fn open_unique_tmp_file(dest: &Path, parent: &Path) -> io::Result<(PathBuf, fs::File)> {
    let file_name = dest
        .file_name()
        .ok_or_else(|| io::Error::other("destination path has no file name"))?;
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
