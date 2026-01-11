use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const CACHE_VERSION: u32 = 1;

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct JmodFingerprint {
    pub(crate) file_name: String,
    pub(crate) len: u64,
    pub(crate) mtime_secs: u64,
    pub(crate) mtime_nanos: u32,
}

impl JmodFingerprint {
    pub(crate) fn for_path(path: &Path) -> std::io::Result<Self> {
        let meta = std::fs::metadata(path)?;
        let modified = meta.modified()?;
        let (mtime_secs, mtime_nanos) = system_time_parts(modified);
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_owned();

        Ok(Self {
            file_name,
            len: meta.len(),
            mtime_secs,
            mtime_nanos,
        })
    }
}

#[derive(Serialize, Deserialize)]
struct SymbolIndexCacheFile {
    version: u32,
    jmods: Vec<JmodFingerprint>,
    class_to_module: HashMap<String, u32>,
    packages_sorted: Vec<String>,
    binary_names_sorted: Vec<String>,
}

pub(crate) struct LoadedSymbolIndex {
    pub(crate) class_to_module: HashMap<String, u32>,
    pub(crate) packages_sorted: Vec<String>,
    pub(crate) binary_names_sorted: Vec<String>,
}

pub(crate) fn fingerprint_jmods(jmod_paths: &[PathBuf]) -> std::io::Result<Vec<JmodFingerprint>> {
    jmod_paths
        .iter()
        .map(|p| JmodFingerprint::for_path(p))
        .collect()
}

pub(crate) fn load_symbol_index(
    cache_dir: &Path,
    jmods_dir: &Path,
    fingerprints: &[JmodFingerprint],
) -> Option<LoadedSymbolIndex> {
    let cache_path = cache_file_path(cache_dir, jmods_dir);
    let bytes = std::fs::read(cache_path).ok()?;
    let file = bincode::deserialize::<SymbolIndexCacheFile>(&bytes).ok()?;

    if file.version != CACHE_VERSION || file.jmods != fingerprints {
        return None;
    }

    Some(LoadedSymbolIndex {
        class_to_module: file.class_to_module,
        packages_sorted: file.packages_sorted,
        binary_names_sorted: file.binary_names_sorted,
    })
}

pub(crate) fn store_symbol_index(
    cache_dir: &Path,
    jmods_dir: &Path,
    fingerprints: Vec<JmodFingerprint>,
    class_to_module: HashMap<String, u32>,
    packages_sorted: Vec<String>,
    binary_names_sorted: Vec<String>,
) -> bool {
    if std::fs::create_dir_all(cache_dir).is_err() {
        return false;
    }

    let cache_path = cache_file_path(cache_dir, jmods_dir);
    let file = SymbolIndexCacheFile {
        version: CACHE_VERSION,
        jmods: fingerprints,
        class_to_module,
        packages_sorted,
        binary_names_sorted,
    };

    let Ok(bytes) = bincode::serialize(&file) else {
        return false;
    };
    atomic_write(&cache_path, &bytes).is_ok()
}

fn cache_file_path(cache_dir: &Path, jmods_dir: &Path) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    jmods_dir.to_string_lossy().hash(&mut hasher);
    let key = hasher.finish();
    cache_dir.join(format!("jdk-symbol-index-{key:016x}.bin"))
}

fn system_time_parts(time: SystemTime) -> (u64, u32) {
    let duration = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| std::time::Duration::from_secs(0));
    (duration.as_secs(), duration.subsec_nanos())
}

fn atomic_write(dest: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };

    fs::create_dir_all(parent)?;

    let (tmp_path, mut file) = open_unique_tmp_file(dest, parent)?;
    let write_result = (|| -> io::Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(err) = write_result {
        drop(file);
        let _ = fs::remove_file(&tmp_path);
        return Err(err);
    }
    drop(file);

    if let Err(err) = rename_overwrite(&tmp_path, dest) {
        let _ = fs::remove_file(&tmp_path);
        return Err(err);
    }

    #[cfg(unix)]
    {
        let _ = fs::File::open(parent).and_then(|dir| dir.sync_all());
    }

    Ok(())
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

fn rename_overwrite(src: &Path, dest: &Path) -> io::Result<()> {
    const MAX_RENAME_ATTEMPTS: usize = 1024;
    let mut attempts = 0usize;

    loop {
        match fs::rename(src, dest) {
            Ok(()) => return Ok(()),
            Err(err)
                if cfg!(windows)
                    && (err.kind() == io::ErrorKind::AlreadyExists || dest.exists()) =>
            {
                match fs::remove_file(dest) {
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
}
