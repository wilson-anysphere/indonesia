use crate::error::CacheError;
use crate::fingerprint::Fingerprint;
use crate::util::{
    atomic_write, bincode_options_limited, bincode_serialize, now_millis,
    BINCODE_PAYLOAD_LIMIT_BYTES,
};
use bincode::Options;
use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};
use std::io::BufReader;
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
        // Don't bother persisting entries that we won't be willing to deserialize
        // later (see `BINCODE_PAYLOAD_LIMIT_BYTES` / `bincode_options_limited`).
        if value.len() > BINCODE_PAYLOAD_LIMIT_BYTES {
            return Ok(());
        }

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
        if bytes.len() > BINCODE_PAYLOAD_LIMIT_BYTES {
            return Ok(());
        }
        atomic_write(&path, &bytes)?;
        self.maybe_gc();
        Ok(())
    }

    pub fn load(&self, key: &str) -> Result<Option<Vec<u8>>, CacheError> {
        let key_fingerprint = Fingerprint::from_bytes(key.as_bytes());
        let path = self.entry_path(&key_fingerprint);
        // Avoid following symlinks out of the cache directory.
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Ok(None),
        };
        if meta.file_type().is_symlink() {
            let _ = std::fs::remove_file(&path);
            return Ok(None);
        }
        if !meta.is_file() {
            return Ok(None);
        }
        if meta.len() > BINCODE_PAYLOAD_LIMIT_BYTES as u64 {
            let _ = std::fs::remove_file(&path);
            return Ok(None);
        }

        let file = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Ok(None),
        };
        let mut reader = BufReader::new(file);

        let schema_version: u32 = match bincode_options_limited().deserialize_from(&mut reader) {
            Ok(v) => v,
            Err(_) => {
                let _ = std::fs::remove_file(&path);
                return Ok(None);
            }
        };
        let nova_version: String = match bincode_options_limited().deserialize_from(&mut reader) {
            Ok(v) => v,
            Err(_) => {
                let _ = std::fs::remove_file(&path);
                return Ok(None);
            }
        };
        let _saved_at_millis: u64 = match bincode_options_limited().deserialize_from(&mut reader) {
            Ok(v) => v,
            Err(_) => {
                let _ = std::fs::remove_file(&path);
                return Ok(None);
            }
        };
        let stored_key: String = match bincode_options_limited().deserialize_from(&mut reader) {
            Ok(v) => v,
            Err(_) => {
                let _ = std::fs::remove_file(&path);
                return Ok(None);
            }
        };
        let stored_fingerprint: Fingerprint =
            match bincode_options_limited().deserialize_from(&mut reader) {
                Ok(v) => v,
                Err(_) => {
                    let _ = std::fs::remove_file(&path);
                    return Ok(None);
                }
            };

        if schema_version != QUERY_DISK_CACHE_SCHEMA_VERSION
            || nova_version != nova_core::NOVA_VERSION
        {
            let _ = std::fs::remove_file(&path);
            return Ok(None);
        }
        if stored_fingerprint != key_fingerprint {
            let _ = std::fs::remove_file(&path);
            return Ok(None);
        }
        if stored_key != key {
            return Ok(None);
        }

        let value: Vec<u8> = match bincode_options_limited().deserialize_from(&mut reader) {
            Ok(v) => v,
            Err(_) => {
                let _ = std::fs::remove_file(&path);
                return Ok(None);
            }
        };

        Ok(Some(value))
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
                if file_type.is_file() || file_type.is_symlink() {
                    let _ = std::fs::remove_file(&path);
                }
                continue;
            }

            if file_type.is_symlink() {
                // Symlinks could point outside the cache directory. Delete them
                // rather than following.
                let _ = std::fs::remove_file(&path);
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
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                let _ = std::fs::remove_file(&path);
                continue;
            };
            if stem != header.key_fingerprint.as_str() {
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

#[derive(Debug, Deserialize)]
struct PersistedQueryValueHeader {
    schema_version: u32,
    nova_version: String,
    saved_at_millis: u64,
    key: IgnoredAny,
    key_fingerprint: Fingerprint,
}

fn read_query_cache_header(path: &Path) -> Option<PersistedQueryValueHeader> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return None;
    }
    if meta.len() > BINCODE_PAYLOAD_LIMIT_BYTES as u64 {
        return None;
    }

    let file = std::fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    bincode_options_limited().deserialize_from(&mut reader).ok()
}
