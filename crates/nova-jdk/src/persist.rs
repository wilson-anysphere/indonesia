use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use nova_cache::Fingerprint;
use nova_storage::{ArtifactKind, Compression, PersistedArchive};

pub(crate) const JDK_SYMBOL_INDEX_SCHEMA_VERSION: u32 = 2;
pub(crate) const CT_SYM_INDEX_SCHEMA_VERSION: u32 = 1;
const CACHE_FILE_NAME: &str = "jdk-symbol-index.idx";
const CT_SYM_CACHE_FILE_PREFIX: &str = "ct-sym-r";
const CACHE_FILE_EXT: &str = "idx";

#[derive(Clone, Debug, PartialEq, Eq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
#[archive(check_bytes)]
pub(crate) struct ContainerFingerprint {
    pub(crate) file_name: String,
    pub(crate) len: u64,
    pub(crate) mtime_secs: u64,
    pub(crate) mtime_nanos: u32,
}

impl ContainerFingerprint {
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
    cache_key_path: String,
    containers: Vec<ContainerFingerprint>,
    class_to_container: Vec<(String, u32)>,
    packages_sorted: Vec<String>,
    binary_names_sorted: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
#[archive(check_bytes)]
struct CtSymIndexCacheFile {
    saved_at_millis: u64,
    ct_sym_path: String,
    fingerprint: ContainerFingerprint,
    release: u32,
    modules: Vec<String>,
    class_to_module: Vec<(String, u32)>,
    internal_to_zip_path: Vec<(String, String)>,
    module_info_zip_paths: Vec<String>,
}

pub(crate) struct LoadedSymbolIndex {
    pub(crate) class_to_container: HashMap<String, u32>,
    pub(crate) packages_sorted: Vec<String>,
    pub(crate) binary_names_sorted: Vec<String>,
}

pub(crate) struct LoadedCtSymIndex {
    pub(crate) modules: Vec<String>,
    pub(crate) class_to_module: HashMap<String, usize>,
    pub(crate) internal_to_zip_path: HashMap<String, String>,
    pub(crate) module_info_zip_paths: Vec<String>,
}

pub(crate) fn fingerprint_containers(
    paths: &[PathBuf],
) -> std::io::Result<Vec<ContainerFingerprint>> {
    paths
        .iter()
        .map(|p| ContainerFingerprint::for_path(p))
        .collect()
}

pub(crate) fn load_symbol_index(
    cache_dir: &Path,
    cache_key_path: &Path,
    fingerprints: &[ContainerFingerprint],
) -> Option<LoadedSymbolIndex> {
    let cache_path = cache_file_path(cache_dir, cache_key_path);
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

    if !fingerprints_match(&archive.containers, fingerprints) {
        return None;
    }

    let container_count = fingerprints.len() as u32;
    let mut class_to_container = HashMap::with_capacity(archive.class_to_container.len());
    for entry in archive.class_to_container.iter() {
        let container_idx = entry.1;
        if container_idx >= container_count {
            let _ = std::fs::remove_file(&cache_path);
            return None;
        }
        class_to_container.insert(entry.0.as_str().to_owned(), container_idx);
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
        class_to_container,
        packages_sorted,
        binary_names_sorted,
    })
}

pub(crate) fn store_symbol_index(
    cache_dir: &Path,
    cache_key_path: &Path,
    fingerprints: Vec<ContainerFingerprint>,
    class_to_container: HashMap<String, u32>,
    packages_sorted: Vec<String>,
    binary_names_sorted: Vec<String>,
) -> bool {
    let cache_path = cache_file_path(cache_dir, cache_key_path);
    let mut class_to_container: Vec<(String, u32)> = class_to_container.into_iter().collect();
    class_to_container.sort_by(|a, b| a.0.cmp(&b.0));

    let file = SymbolIndexCacheFile {
        saved_at_millis: now_millis(),
        cache_key_path: cache_key_path.to_string_lossy().to_string(),
        containers: fingerprints,
        class_to_container,
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

pub(crate) fn load_ct_sym_index(
    cache_dir: &Path,
    ct_sym_path: &Path,
    release: u32,
    fingerprint: &ContainerFingerprint,
) -> Option<LoadedCtSymIndex> {
    let cache_path = ct_sym_cache_file_path(cache_dir, ct_sym_path, release);
    let archive = match PersistedArchive::<CtSymIndexCacheFile>::open_optional(
        &cache_path,
        ArtifactKind::JdkSymbolIndex,
        CT_SYM_INDEX_SCHEMA_VERSION,
    ) {
        Ok(Some(archive)) => archive,
        Ok(None) => return None,
        Err(_) => {
            let _ = std::fs::remove_file(&cache_path);
            return None;
        }
    };

    if archive.release != release {
        return None;
    }

    // The cache is keyed by a fingerprinted directory path; validate that the
    // intended ct.sym file still matches.
    if !fingerprint_matches_single(&archive.fingerprint, fingerprint) {
        return None;
    }

    // Optional extra safety check: ensure the stored ct.sym path still matches.
    let current_path_str = canonical_path_string(ct_sym_path);
    if archive.ct_sym_path.as_str() != current_path_str {
        return None;
    }

    let modules: Vec<String> = archive
        .modules
        .iter()
        .map(|m| m.as_str().to_owned())
        .collect();
    let module_count = modules.len() as u32;

    let mut internal_to_zip_path = HashMap::with_capacity(archive.internal_to_zip_path.len());
    for (internal, zip_path) in archive.internal_to_zip_path.iter() {
        internal_to_zip_path.insert(internal.as_str().to_owned(), zip_path.as_str().to_owned());
    }

    let mut class_to_module = HashMap::with_capacity(archive.class_to_module.len());
    for (internal, module_idx) in archive.class_to_module.iter() {
        if *module_idx >= module_count {
            let _ = std::fs::remove_file(&cache_path);
            return None;
        }
        let internal = internal.as_str().to_owned();
        if !internal_to_zip_path.contains_key(&internal) {
            let _ = std::fs::remove_file(&cache_path);
            return None;
        }
        class_to_module.insert(internal, *module_idx as usize);
    }

    let module_info_zip_paths = archive
        .module_info_zip_paths
        .iter()
        .map(|p| p.as_str().to_owned())
        .collect();

    Some(LoadedCtSymIndex {
        modules,
        class_to_module,
        internal_to_zip_path,
        module_info_zip_paths,
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn store_ct_sym_index(
    cache_dir: &Path,
    ct_sym_path: &Path,
    release: u32,
    fingerprint: ContainerFingerprint,
    modules: Vec<String>,
    class_to_module: &mut HashMap<String, usize>,
    internal_to_zip_path: &mut HashMap<String, String>,
    mut module_info_zip_paths: Vec<String>,
) -> bool {
    let cache_path = ct_sym_cache_file_path(cache_dir, ct_sym_path, release);

    let mut class_to_module_vec: Vec<(String, u32)> = std::mem::take(class_to_module)
        .into_iter()
        .map(|(k, v)| (k, v as u32))
        .collect();
    class_to_module_vec.sort_by(|a, b| a.0.cmp(&b.0));

    let mut internal_to_zip_path_vec: Vec<(String, String)> =
        std::mem::take(internal_to_zip_path).into_iter().collect();
    internal_to_zip_path_vec.sort_by(|a, b| a.0.cmp(&b.0));

    module_info_zip_paths.sort();

    let file = CtSymIndexCacheFile {
        saved_at_millis: now_millis(),
        ct_sym_path: canonical_path_string(ct_sym_path),
        fingerprint,
        release,
        modules,
        class_to_module: class_to_module_vec,
        internal_to_zip_path: internal_to_zip_path_vec,
        module_info_zip_paths,
    };

    let ok = nova_storage::write_archive_atomic(
        &cache_path,
        ArtifactKind::JdkSymbolIndex,
        CT_SYM_INDEX_SCHEMA_VERSION,
        &file,
        Compression::None,
    )
    .is_ok();

    // Restore the maps regardless of whether the write succeeded so the caller
    // can keep using the computed index.
    let CtSymIndexCacheFile {
        class_to_module: class_to_module_entries,
        internal_to_zip_path: internal_to_zip_path_entries,
        ..
    } = file;
    *class_to_module = class_to_module_entries
        .into_iter()
        .map(|(k, v)| (k, v as usize))
        .collect();
    *internal_to_zip_path = internal_to_zip_path_entries.into_iter().collect();

    ok
}

fn cache_file_path(cache_dir: &Path, cache_key_path: &Path) -> PathBuf {
    cache_dir_path(cache_dir, cache_key_path).join(CACHE_FILE_NAME)
}

fn ct_sym_cache_file_path(cache_dir: &Path, ct_sym_path: &Path, release: u32) -> PathBuf {
    let key_path = ct_sym_cache_key_path(ct_sym_path);
    let file_name = format!("{CT_SYM_CACHE_FILE_PREFIX}{release}.{CACHE_FILE_EXT}");
    cache_dir_path(cache_dir, &key_path).join(file_name)
}

fn cache_dir_path(cache_dir: &Path, cache_key_path: &Path) -> PathBuf {
    let canonical = cache_key_path.to_string_lossy().replace('\\', "/");
    let fingerprint = Fingerprint::from_bytes(canonical.as_bytes());
    cache_dir.join(fingerprint.as_str())
}

fn ct_sym_cache_key_path(ct_sym_path: &Path) -> PathBuf {
    // The canonical cache key is the JDK root directory so ct.sym and jmod
    // caches share the same `<cache>/<jdk-hash>/` directory.
    let mut key = ct_sym_path.to_path_buf();
    if ct_sym_path
        .file_name()
        .is_some_and(|n| n == std::ffi::OsStr::new("ct.sym"))
    {
        if let Some(lib_dir) = ct_sym_path.parent() {
            if lib_dir
                .file_name()
                .is_some_and(|n| n == std::ffi::OsStr::new("lib"))
            {
                if let Some(root) = lib_dir.parent() {
                    key = root.to_path_buf();
                }
            }
        }
    }

    std::fs::canonicalize(&key).unwrap_or(key)
}

fn canonical_path_string(path: &Path) -> String {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    path.to_string_lossy().replace('\\', "/")
}

fn system_time_parts(time: SystemTime) -> (u64, u32) {
    let duration = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| std::time::Duration::from_secs(0));
    (duration.as_secs(), duration.subsec_nanos())
}

fn fingerprints_match(
    archived: &rkyv::vec::ArchivedVec<ArchivedContainerFingerprint>,
    current: &[ContainerFingerprint],
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

fn fingerprint_matches_single(
    archived: &ArchivedContainerFingerprint,
    current: &ContainerFingerprint,
) -> bool {
    archived.file_name.as_str() == current.file_name
        && archived.len == current.len
        && archived.mtime_secs == current.mtime_secs
        && archived.mtime_nanos == current.mtime_nanos
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}
