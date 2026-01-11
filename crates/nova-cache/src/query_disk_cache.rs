use crate::error::CacheError;
use crate::fingerprint::Fingerprint;
use crate::util::{
    atomic_write, bincode_deserialize, bincode_options_limited, bincode_serialize, now_millis,
    read_file_limited,
};
use bincode::Options;
use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub const QUERY_DISK_CACHE_SCHEMA_VERSION: u32 = 1;

/// Best-effort on-disk persistence for `nova-db::QueryCache`.
///
/// The cache is versioned (schema + Nova version), written atomically, and
/// guarded against key collisions by storing the full key alongside the
/// fingerprint-based filename.
#[derive(Clone, Debug)]
pub struct QueryDiskCache {
    root: PathBuf,
    policy: QueryDiskCachePolicy,
    last_gc_millis: Arc<AtomicU64>,
}

#[derive(Clone, Copy, Debug)]
pub struct QueryDiskCachePolicy {
    /// Time-to-live for entries, measured from the persisted `saved_at_millis`.
    pub ttl_millis: u64,
    /// Maximum total size of cache files on disk.
    pub max_bytes: u64,
    /// Minimum time between GC runs.
    pub gc_interval_millis: u64,
}

impl Default for QueryDiskCachePolicy {
    fn default() -> Self {
        // Conservative defaults:
        // - 7 days of TTL keeps warm-start value without growing forever.
        // - 512MB max bounds disk usage even with large query payloads.
        // - GC interval avoids scanning the directory on every write.
        Self {
            ttl_millis: 7 * 24 * 60 * 60 * 1000,
            max_bytes: 512 * 1024 * 1024,
            gc_interval_millis: 5 * 60 * 1000,
        }
    }
}

impl QueryDiskCache {
    pub fn new(root: impl AsRef<Path>) -> Result<Self, CacheError> {
        Self::new_with_policy(root, QueryDiskCachePolicy::default())
    }

    pub fn new_with_policy(
        root: impl AsRef<Path>,
        policy: QueryDiskCachePolicy,
    ) -> Result<Self, CacheError> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root)?;
        let cache = Self {
            root,
            policy,
            last_gc_millis: Arc::new(AtomicU64::new(0)),
        };
        // Best-effort: cache should still work even if GC fails.
        let _ = cache.gc();
        Ok(cache)
    }

    pub fn store(&self, key: &str, value: &[u8]) -> Result<(), CacheError> {
        let key_fingerprint = Fingerprint::from_bytes(key.as_bytes());
        let path = self.entry_path(&key_fingerprint);
        let persisted = PersistedQueryValue {
            schema_version: QUERY_DISK_CACHE_SCHEMA_VERSION,
            nova_version: nova_core::NOVA_VERSION.to_string(),
            saved_at_millis: now_millis(),
            key,
            key_fingerprint,
            value,
        };

        let bytes = bincode_serialize(&persisted)?;
        atomic_write(&path, &bytes)?;
        self.maybe_gc();
        Ok(())
    }

    pub fn load(&self, key: &str) -> Result<Option<Vec<u8>>, CacheError> {
        let key_fingerprint = Fingerprint::from_bytes(key.as_bytes());
        let path = self.entry_path(&key_fingerprint);
        let bytes = match read_file_limited(&path) {
            Some(bytes) => bytes,
            None => return Ok(None),
        };

        let persisted: PersistedQueryValueOwned = match bincode_deserialize(&bytes) {
            Ok(value) => value,
            Err(_) => {
                let _ = std::fs::remove_file(&path);
                return Ok(None);
            }
        };

        if persisted.schema_version != QUERY_DISK_CACHE_SCHEMA_VERSION
            || persisted.nova_version != nova_core::NOVA_VERSION
        {
            // The file exists but is not usable for this version; treat as a miss and
            // delete it so we don't keep growing stale caches.
            let _ = std::fs::remove_file(&path);
            return Ok(None);
        }

        if persisted.key_fingerprint != key_fingerprint {
            // The entry doesn't match the file name; treat as corruption and delete it.
            let _ = std::fs::remove_file(&path);
            return Ok(None);
        }

        if persisted.key != key {
            // Fingerprint collisions should be treated as a miss, but we do **not**
            // delete the file: in the (extremely unlikely) event of a collision, we
            // don't want reads for one key to erase the other key's cached value.
            return Ok(None);
        }

        Ok(Some(persisted.value))
    }

    fn entry_path(&self, fingerprint: &Fingerprint) -> PathBuf {
        self.root.join(format!("{}.bin", fingerprint.as_str()))
    }

    fn maybe_gc(&self) {
        let now = now_millis();
        let last = self.last_gc_millis.load(Ordering::Relaxed);
        if now.saturating_sub(last) < self.policy.gc_interval_millis {
            return;
        }
        if self
            .last_gc_millis
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        let _ = self.gc();
    }

    pub fn gc(&self) -> Result<(), CacheError> {
        let now = now_millis();
        let mut candidates: Vec<GcEntry> = Vec::new();
        let mut total_bytes: u64 = 0;

        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(CacheError::from(err)),
        };

        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            let path = entry.path();
            let meta = match std::fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                Err(_) => continue,
            };

            let file_type = meta.file_type();
            if !file_type.is_file() && !file_type.is_symlink() {
                continue;
            }

            // We only expect `.bin` files for cache entries. Clean up any other
            // leftovers (including crashed atomic-write tempfiles).
            if path.extension().and_then(|s| s.to_str()) != Some("bin") {
                if file_type.is_file() {
                    let _ = std::fs::remove_file(&path);
                }
                continue;
            }

            let Some(header) = read_query_cache_header(&path) else {
                // Corrupted or unreadable cache entry (including payloads over our
                // deserialization limit). Treat as stale and delete it.
                let _ = std::fs::remove_file(&path);
                continue;
            };

            // Version-gate entries at GC time too so older Nova versions don't
            // accumulate indefinitely if they're never read.
            if header.schema_version != QUERY_DISK_CACHE_SCHEMA_VERSION
                || header.nova_version != nova_core::NOVA_VERSION
            {
                let _ = std::fs::remove_file(&path);
                continue;
            }

            // If the file name doesn't match the stored key fingerprint, treat
            // as corruption and delete.
            if path
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|stem| stem != header.key_fingerprint.as_str())
            {
                let _ = std::fs::remove_file(&path);
                continue;
            }

            if now.saturating_sub(header.saved_at_millis) > self.policy.ttl_millis {
                let _ = std::fs::remove_file(&path);
                continue;
            }

            let len = meta.len();
            total_bytes = total_bytes.saturating_add(len);
            candidates.push(GcEntry {
                last_used_millis: header.saved_at_millis,
                len,
                path,
            });
        }

        if total_bytes <= self.policy.max_bytes {
            return Ok(());
        }

        // Evict oldest files first until we're within budget.
        candidates.sort_by_key(|entry| entry.last_used_millis);
        for entry in candidates {
            if total_bytes <= self.policy.max_bytes {
                break;
            }
            if std::fs::remove_file(&entry.path).is_ok() {
                total_bytes = total_bytes.saturating_sub(entry.len);
            }
        }

        Ok(())
    }
}

#[derive(Debug)]
struct GcEntry {
    last_used_millis: u64,
    len: u64,
    path: PathBuf,
}

#[derive(Debug, Serialize)]
struct PersistedQueryValue<'a> {
    schema_version: u32,
    nova_version: String,
    saved_at_millis: u64,
    key: &'a str,
    key_fingerprint: Fingerprint,
    value: &'a [u8],
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedQueryValueOwned {
    schema_version: u32,
    nova_version: String,
    saved_at_millis: u64,
    key: String,
    key_fingerprint: Fingerprint,
    value: Vec<u8>,
}

#[derive(Debug, Deserialize)]
struct PersistedQueryValueHeader {
    schema_version: u32,
    nova_version: String,
    saved_at_millis: u64,
    key: IgnoredAny,
    key_fingerprint: Fingerprint,
    value: IgnoredAny,
}

fn read_query_cache_header(path: &Path) -> Option<PersistedQueryValueHeader> {
    let bytes = read_file_limited(path)?;
    // Use the same bincode options as the writer (and the same size cap as
    // regular loads) to avoid allocating unbounded amounts of memory if the
    // file is corrupted.
    let mut cursor = Cursor::new(bytes);
    bincode_options_limited().deserialize_from(&mut cursor).ok()
}
