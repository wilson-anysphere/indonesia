use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const CACHE_VERSION: u32 = 1;

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
    std::fs::write(cache_path, bytes).is_ok()
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
