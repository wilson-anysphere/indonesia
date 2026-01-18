use crate::error::CacheError;
use crate::metadata::{
    CacheMetadataArchive, CACHE_METADATA_BIN_FILENAME, CACHE_METADATA_JSON_FILENAME,
};
use crate::util::now_millis;
use crate::CacheConfig;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

#[derive(Debug, Deserialize)]
struct CacheMetadataSummary {
    schema_version: Option<u32>,
    nova_version: Option<String>,
    last_updated_millis: Option<u64>,
}

/// Information about a single per-project cache directory under the global cache root.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProjectCacheInfo {
    /// Directory name under the global cache root (typically a project hash).
    pub name: String,
    /// Full on-disk path to the cache directory.
    pub path: PathBuf,
    /// Best-effort size on disk (bytes).
    pub size_bytes: u64,
    /// Last time the cache was updated, if known.
    ///
    /// This is derived from `metadata.bin`/`metadata.json` (preferred) and/or filesystem timestamps
    /// for `perf.json` when available.
    pub last_updated_millis: Option<u64>,
    /// Nova version recorded in cache metadata (if available).
    pub nova_version: Option<String>,
    /// Cache metadata schema version recorded in cache metadata (if available).
    pub schema_version: Option<u32>,
}

/// Policy for garbage-collecting global per-project caches.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CacheGcPolicy {
    /// Maximum total disk usage allowed for per-project caches.
    pub max_total_bytes: u64,
    /// Optional maximum age for per-project caches. Caches older than this (based
    /// on `last_updated_millis`) are removed first.
    pub max_age_ms: Option<u64>,
    /// Number of most-recently-updated caches to always keep.
    pub keep_latest_n: usize,
}

/// Result summary from a GC run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CacheGcReport {
    pub before_total_bytes: u64,
    pub after_total_bytes: u64,
    /// Best-effort number of bytes freed (sum of `size_bytes` for deleted caches).
    pub deleted_bytes: u64,
    /// Number of cache directories successfully removed.
    pub deleted_caches: usize,
    pub deleted: Vec<ProjectCacheInfo>,
    pub failed: Vec<CacheGcFailure>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CacheGcFailure {
    pub cache: ProjectCacheInfo,
    pub error: String,
}

/// Returns the global cache root (e.g. `~/.nova/cache`) honoring `CacheConfig` / `NOVA_CACHE_DIR`.
pub fn cache_root(config: &CacheConfig) -> Result<PathBuf, CacheError> {
    Ok(match &config.cache_root_override {
        Some(root) => root.clone(),
        None => crate::cache_dir::default_cache_root()?,
    })
}

/// Enumerate all per-project caches under `cache_root`, excluding the shared `deps/` cache.
pub fn enumerate_project_caches(
    cache_root: impl AsRef<Path>,
) -> Result<Vec<ProjectCacheInfo>, CacheError> {
    let cache_root = cache_root.as_ref();
    if !cache_root.exists() {
        return Ok(Vec::new());
    }

    let mut logged_metadata_bin_stat_error = false;
    let mut logged_metadata_json_stat_error = false;
    let mut logged_metadata_json_open_error = false;
    let mut logged_metadata_json_parse_error = false;
    let mut logged_perf_stat_error = false;

    let mut caches = Vec::new();
    for entry in std::fs::read_dir(cache_root)? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                tracing::debug!(
                    target = "nova.cache",
                    cache_root = %cache_root.display(),
                    error = %err,
                    "failed to read cache root directory entry while enumerating caches"
                );
                continue;
            }
        };
        let name_os = entry.file_name();
        if name_os == "deps" {
            continue;
        }

        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(err) => {
                // Cache entries can race with deletion; only log unexpected errors.
                if err.kind() != std::io::ErrorKind::NotFound {
                    let path = entry.path();
                    tracing::debug!(
                        target = "nova.cache",
                        path = %path.display(),
                        error = %err,
                        "failed to read cache entry file type while enumerating caches"
                    );
                }
                continue;
            }
        };
        if !(file_type.is_dir() || file_type.is_symlink()) {
            continue;
        }

        // NOTE: `read_dir` entries are direct children of `cache_root`, but we still
        // validate to avoid path surprises if callers pass inconsistent roots.
        let path = entry.path();
        if path.strip_prefix(cache_root).is_err() {
            continue;
        }

        let name = name_os.to_string_lossy().to_string();

        let mut last_updated_millis = None;
        let mut nova_version = None;
        let mut schema_version = None;

        // Never follow symlinks when inspecting cache contents. If the top-level
        // project entry is a symlink, treat it as opaque and allow GC to remove
        // the symlink itself.
        if !file_type.is_symlink() {
            let metadata_path = path.join(CACHE_METADATA_JSON_FILENAME);
            let metadata_bin_path = path.join(CACHE_METADATA_BIN_FILENAME);

            let metadata_bin_is_file = match std::fs::symlink_metadata(&metadata_bin_path) {
                Ok(meta) => meta.is_file(),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
                Err(err) => {
                    if !logged_metadata_bin_stat_error {
                        logged_metadata_bin_stat_error = true;
                        tracing::debug!(
                            target = "nova.cache",
                            path = %metadata_bin_path.display(),
                            error = %err,
                            "failed to stat cache metadata archive while enumerating caches"
                        );
                    }
                    false
                }
            };

            let mut loaded_metadata = false;
            if metadata_bin_is_file {
                if let Some(metadata) = CacheMetadataArchive::open(&metadata_bin_path)? {
                    last_updated_millis = Some(metadata.last_updated_millis());
                    nova_version = Some(metadata.nova_version().to_string());
                    schema_version = Some(metadata.schema_version());
                    loaded_metadata = true;
                }
            }

            let metadata_json_is_file = match std::fs::symlink_metadata(&metadata_path) {
                Ok(meta) => meta.is_file(),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
                Err(err) => {
                    if !logged_metadata_json_stat_error {
                        logged_metadata_json_stat_error = true;
                        tracing::debug!(
                            target = "nova.cache",
                            path = %metadata_path.display(),
                            error = %err,
                            "failed to stat cache metadata JSON while enumerating caches"
                        );
                    }
                    false
                }
            };

            if !loaded_metadata && metadata_json_is_file {
                match std::fs::File::open(&metadata_path) {
                    Ok(file) => {
                        let reader = std::io::BufReader::new(file);
                        match serde_json::from_reader::<_, CacheMetadataSummary>(reader) {
                            Ok(metadata) => {
                                last_updated_millis = metadata.last_updated_millis;
                                nova_version = metadata.nova_version;
                                schema_version = metadata.schema_version;
                            }
                            Err(err) => {
                                if !logged_metadata_json_parse_error {
                                    logged_metadata_json_parse_error = true;
                                    tracing::debug!(
                                        target = "nova.cache",
                                        path = %metadata_path.display(),
                                        error = %err,
                                        "failed to parse cache metadata JSON while enumerating caches"
                                    );
                                }
                            }
                        }
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => {
                        if !logged_metadata_json_open_error {
                            logged_metadata_json_open_error = true;
                            tracing::debug!(
                                target = "nova.cache",
                                path = %metadata_path.display(),
                                error = %err,
                                "failed to open cache metadata JSON while enumerating caches"
                            );
                        }
                    }
                }
            }

            let perf_path = path.join("perf.json");
            match std::fs::symlink_metadata(&perf_path) {
                Ok(meta) => {
                    if meta.is_file() {
                        if let Some(perf_updated) = modified_millis(&perf_path) {
                            last_updated_millis = Some(match last_updated_millis {
                                Some(prev) => prev.max(perf_updated),
                                None => perf_updated,
                            });
                        }
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => {
                    if !logged_perf_stat_error {
                        logged_perf_stat_error = true;
                        tracing::debug!(
                            target = "nova.cache",
                            path = %perf_path.display(),
                            error = %err,
                            "failed to stat perf.json while enumerating caches"
                        );
                    }
                }
            }
        }

        if last_updated_millis.is_none() {
            // Best-effort fallback: use the directory/symlink mtime. This avoids treating
            // brand-new (but not yet fully initialized) cache directories as stale.
            last_updated_millis = modified_millis(&path);
        }

        let size_bytes = dir_size_bytes_nofollow(&path);

        caches.push(ProjectCacheInfo {
            name,
            path,
            size_bytes,
            last_updated_millis,
            nova_version,
            schema_version,
        });
    }

    // Deterministic ordering.
    caches.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(caches)
}

/// Enumerate all per-project caches under the configured global cache root.
pub fn enumerate_project_caches_from_config(
    config: &CacheConfig,
) -> Result<Vec<ProjectCacheInfo>, CacheError> {
    let root = cache_root(config)?;
    enumerate_project_caches(root)
}

/// Garbage-collect per-project caches under `cache_root`.
///
/// The GC algorithm:
/// - never deletes `deps/`
/// - determines a "protected set" of the `keep_latest_n` newest caches
/// - deletes stale caches first when `max_age_ms` is set (oldest-first)
/// - then deletes additional caches oldest-first until `max_total_bytes` is met
///
/// Cache directories are removed "atomically" by first renaming them to a unique sibling
/// path and then deleting the renamed directory without following symlinks.
pub fn gc_project_caches(
    cache_root: impl AsRef<Path>,
    policy: &CacheGcPolicy,
) -> Result<CacheGcReport, CacheError> {
    let cache_root = cache_root.as_ref();
    let mut caches = enumerate_project_caches(cache_root)?;

    let mut before_total_bytes = 0_u64;
    for cache in &caches {
        before_total_bytes = before_total_bytes.saturating_add(cache.size_bytes);
    }

    let now = now_millis();

    // Determine the "protected" set of newest caches.
    let mut newest = caches.clone();
    newest.sort_by(|a, b| {
        let a_ts = a.last_updated_millis.unwrap_or(0);
        let b_ts = b.last_updated_millis.unwrap_or(0);
        b_ts.cmp(&a_ts).then_with(|| a.name.cmp(&b.name))
    });

    let mut protected = HashSet::<String>::new();
    for cache in newest.into_iter().take(policy.keep_latest_n) {
        protected.insert(cache.name);
    }

    // Oldest-first candidates excluding protected caches.
    caches.retain(|c| !protected.contains(&c.name));
    caches.sort_by(|a, b| {
        let a_ts = a.last_updated_millis.unwrap_or(0);
        let b_ts = b.last_updated_millis.unwrap_or(0);
        a_ts.cmp(&b_ts).then_with(|| a.name.cmp(&b.name))
    });

    let mut remaining_bytes = before_total_bytes;
    let mut deleted = Vec::new();
    let mut failed = Vec::new();
    let mut deleted_names = HashSet::<String>::new();

    if let Some(max_age_ms) = policy.max_age_ms {
        for cache in &caches {
            if protected.contains(&cache.name) {
                continue;
            }

            if !is_stale(cache.last_updated_millis, now, max_age_ms) {
                continue;
            }

            match delete_cache_dir(cache_root, cache) {
                Ok(()) => {
                    remaining_bytes = remaining_bytes.saturating_sub(cache.size_bytes);
                    deleted_names.insert(cache.name.clone());
                    deleted.push(cache.clone());
                }
                Err(err) => {
                    failed.push(CacheGcFailure {
                        cache: cache.clone(),
                        error: err.to_string(),
                    });
                }
            }
        }
    }

    for cache in &caches {
        if remaining_bytes <= policy.max_total_bytes {
            break;
        }
        if deleted_names.contains(&cache.name) {
            continue;
        }

        match delete_cache_dir(cache_root, cache) {
            Ok(()) => {
                remaining_bytes = remaining_bytes.saturating_sub(cache.size_bytes);
                deleted_names.insert(cache.name.clone());
                deleted.push(cache.clone());
            }
            Err(err) => {
                failed.push(CacheGcFailure {
                    cache: cache.clone(),
                    error: err.to_string(),
                });
            }
        }
    }

    let deleted_bytes = deleted
        .iter()
        .fold(0u64, |acc, cache| acc.saturating_add(cache.size_bytes));

    Ok(CacheGcReport {
        before_total_bytes,
        after_total_bytes: remaining_bytes,
        deleted_bytes,
        deleted_caches: deleted.len(),
        deleted,
        failed,
    })
}

/// GC per-project caches under the configured global cache root.
pub fn gc_project_caches_from_config(
    config: &CacheConfig,
    policy: &CacheGcPolicy,
) -> Result<CacheGcReport, CacheError> {
    let root = cache_root(config)?;
    gc_project_caches(root, policy)
}

fn is_stale(last_updated_millis: Option<u64>, now_millis: u64, max_age_ms: u64) -> bool {
    let Some(last) = last_updated_millis else {
        // If we can't determine recency, treat it as stale so GC can clean it up.
        return true;
    };
    now_millis.saturating_sub(last) > max_age_ms
}

fn delete_cache_dir(cache_root: &Path, cache: &ProjectCacheInfo) -> Result<(), CacheError> {
    validate_under_root(cache_root, &cache.path)?;

    let meta = match std::fs::symlink_metadata(&cache.path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    if meta.file_type().is_symlink() {
        // Best-effort: remove the symlink itself, never follow it.
        remove_symlink_best_effort(&cache.path)?;
        return Ok(());
    }

    if !meta.is_dir() {
        // Unexpected cache entry; nothing to do.
        return Ok(());
    }

    let parent = cache
        .path
        .parent()
        .ok_or_else(|| CacheError::InvalidArchivePath {
            path: cache.path.clone(),
        })?;

    let trash = unique_sibling_path(parent, &cache.name, "gc");
    match std::fs::rename(&cache.path, &trash) {
        Ok(()) => match remove_dir_all_nofollow(&trash) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(CacheError::Io(err)),
        },
        Err(_) => {
            // Fall back to removing in place if the rename fails (e.g. Windows file locks).
            match remove_dir_all_nofollow(&cache.path) {
                Ok(()) => Ok(()),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(err) => Err(CacheError::Io(err)),
            }
        }
    }
}

fn validate_under_root(cache_root: &Path, path: &Path) -> Result<(), CacheError> {
    // Lexical check only; do not follow symlinks.
    if path.strip_prefix(cache_root).is_err() {
        return Err(CacheError::PathNotUnderCacheRoot {
            path: path.to_path_buf(),
            cache_root: cache_root.to_path_buf(),
        });
    }
    Ok(())
}

fn unique_sibling_path(parent: &Path, name: &str, suffix: &str) -> PathBuf {
    let pid = std::process::id();
    let ts = now_millis();
    for attempt in 0..1000u32 {
        let candidate = parent.join(format!("{name}.{suffix}-{pid}-{ts}-{attempt}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    parent.join(format!("{name}.{suffix}-{pid}-{ts}"))
}

fn modified_millis(path: &Path) -> Option<u64> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) => {
            // Cache entries can race with deletion; only log unexpected errors.
            if err.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!(
                    target = "nova.cache",
                    path = %path.display(),
                    error = %err,
                    "failed to stat file while reading modified time"
                );
            }
            return None;
        }
    };

    let modified = match meta.modified() {
        Ok(modified) => modified,
        Err(err) => {
            tracing::debug!(
                target = "nova.cache",
                path = %path.display(),
                error = %err,
                "failed to read file modified time"
            );
            return None;
        }
    };

    match modified.duration_since(UNIX_EPOCH) {
        Ok(d) => Some(d.as_millis() as u64),
        Err(err) => {
            tracing::debug!(
                target = "nova.cache",
                path = %path.display(),
                error = %err,
                "file modified time predates unix epoch"
            );
            None
        }
    }
}

fn dir_size_bytes_nofollow(root: &Path) -> u64 {
    let mut total = 0_u64;
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                let io_err = err.io_error();
                let should_log = match io_err {
                    Some(io_err) => io_err.kind() != std::io::ErrorKind::NotFound,
                    None => true,
                };
                if should_log {
                    let path = err.path().map(|p| p.display().to_string());
                    tracing::debug!(
                        target = "nova.cache",
                        path,
                        error = %err,
                        "failed to walk cache directory while computing size"
                    );
                }
                continue;
            }
        };
        let ty = entry.file_type();
        if !(ty.is_file() || ty.is_symlink()) {
            continue;
        }
        let len = match std::fs::symlink_metadata(entry.path()) {
            Ok(meta) => meta.len(),
            Err(err) => {
                // Cache entries can race with deletion; only log unexpected errors.
                if err.kind() != std::io::ErrorKind::NotFound {
                    tracing::debug!(
                        target = "nova.cache",
                        path = %entry.path().display(),
                        error = %err,
                        "failed to stat cache entry while computing size"
                    );
                }
                continue;
            }
        };
        total = total.saturating_add(len);
    }
    total
}

fn remove_dir_all_nofollow(path: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() || meta.is_file() {
        return remove_symlink_best_effort(path);
    }
    if !meta.is_dir() {
        // Best-effort for unknown file types.
        return remove_symlink_best_effort(path);
    }

    for entry in walkdir::WalkDir::new(path)
        .follow_links(false)
        .contents_first(true)
    {
        let entry = entry.map_err(std::io::Error::other)?;
        let ty = entry.file_type();
        if ty.is_dir() {
            std::fs::remove_dir(entry.path())?;
        } else {
            match std::fs::remove_file(entry.path()) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::IsADirectory => {
                    std::fs::remove_dir(entry.path())?
                }
                Err(err) => return Err(err),
            }
        }
    }
    Ok(())
}

fn remove_symlink_best_effort(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::IsADirectory => std::fs::remove_dir(path),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}
