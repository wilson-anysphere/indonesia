use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use nova_cache::Fingerprint;
use nova_storage::{ArtifactKind, Compression, PersistedArchive};

pub(crate) const JDK_SYMBOL_INDEX_SCHEMA_VERSION: u32 = 1;
const CACHE_FILE_NAME: &str = "jdk-symbol-index.idx";

#[derive(Clone, Debug, PartialEq, Eq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
#[archive(check_bytes)]
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

#[derive(Clone, Debug, PartialEq, Eq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
#[archive(check_bytes)]
struct SymbolIndexCacheFile {
    saved_at_millis: u64,
    jmods_dir: String,
    jmods: Vec<JmodFingerprint>,
    class_to_module: Vec<(String, u32)>,
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
    let archive = match PersistedArchive::<SymbolIndexCacheFile>::open_optional(
        &cache_path,
        ArtifactKind::JdkSymbolIndex,
        JDK_SYMBOL_INDEX_SCHEMA_VERSION,
    ) {
        Ok(Some(archive)) => archive,
        Ok(None) => return None,
        Err(_) => {
            let _ = std::fs::remove_file(&cache_path);
            return None;
        }
    };

    if !fingerprints_match(&archive.jmods, fingerprints) {
        return None;
    }

    let module_count = fingerprints.len() as u32;
    let mut class_to_module = HashMap::with_capacity(archive.class_to_module.len());
    for entry in archive.class_to_module.iter() {
        let module_idx = entry.1;
        if module_idx >= module_count {
            let _ = std::fs::remove_file(&cache_path);
            return None;
        }
        class_to_module.insert(entry.0.as_str().to_owned(), module_idx);
    }

    let packages_sorted: Vec<String> = archive
        .packages_sorted
        .iter()
        .map(|p| p.as_str().to_owned())
        .collect();
    let binary_names_sorted: Vec<String> = archive
        .binary_names_sorted
        .iter()
        .map(|n| n.as_str().to_owned())
        .collect();

    Some(LoadedSymbolIndex {
        class_to_module,
        packages_sorted,
        binary_names_sorted,
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
    let cache_path = cache_file_path(cache_dir, jmods_dir);
    let mut class_to_module: Vec<(String, u32)> = class_to_module.into_iter().collect();
    class_to_module.sort_by(|a, b| a.0.cmp(&b.0));

    let file = SymbolIndexCacheFile {
        saved_at_millis: now_millis(),
        jmods_dir: jmods_dir.to_string_lossy().to_string(),
        jmods: fingerprints,
        class_to_module,
        packages_sorted,
        binary_names_sorted,
    };
    nova_storage::write_archive_atomic(
        &cache_path,
        ArtifactKind::JdkSymbolIndex,
        JDK_SYMBOL_INDEX_SCHEMA_VERSION,
        &file,
        Compression::None,
    )
    .is_ok()
}

fn cache_file_path(cache_dir: &Path, jmods_dir: &Path) -> PathBuf {
    let canonical = jmods_dir.to_string_lossy().replace('\\', "/");
    let fingerprint = Fingerprint::from_bytes(canonical.as_bytes());
    cache_dir
        .join(fingerprint.as_str())
        .join(CACHE_FILE_NAME)
}

fn system_time_parts(time: SystemTime) -> (u64, u32) {
    let duration = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| std::time::Duration::from_secs(0));
    (duration.as_secs(), duration.subsec_nanos())
}

fn fingerprints_match(
    archived: &rkyv::vec::ArchivedVec<ArchivedJmodFingerprint>,
    current: &[JmodFingerprint],
) -> bool {
    if archived.len() != current.len() {
        return false;
    }

    for (archived, current) in archived.iter().zip(current) {
        if archived.file_name.as_str() != current.file_name
            || archived.len != current.len
            || archived.mtime_secs != current.mtime_secs
            || archived.mtime_nanos != current.mtime_nanos
        {
            return false;
        }
    }

    true
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}
