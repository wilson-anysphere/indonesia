use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{ClasspathClassStub, ClasspathEntry, ClasspathError, ClasspathFingerprint};

/// Schema version for per-entry classpath stub caches stored under a project cache directory.
///
/// This is the schema version embedded in the `nova-storage` header.
///
/// Version history:
/// - 1: legacy `bincode` payloads (no `nova-storage` header).
/// - 2: `nova-storage` (`rkyv`) archives with validation + size caps.
const CLASSPATH_ENTRY_CACHE_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
#[archive(check_bytes)]
#[repr(u8)]
enum CachedClasspathEntryKind {
    ClassDir = 1,
    Jar = 2,
    Jmod = 3,
}

#[derive(Debug, Clone, PartialEq, Eq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
#[archive(check_bytes)]
struct CachedClasspathEntry {
    kind: CachedClasspathEntryKind,
    path: String,
}

impl CachedClasspathEntry {
    fn from_entry(entry: &ClasspathEntry) -> Self {
        let kind = match entry {
            ClasspathEntry::ClassDir(_) => CachedClasspathEntryKind::ClassDir,
            ClasspathEntry::Jar(_) => CachedClasspathEntryKind::Jar,
            ClasspathEntry::Jmod(_) => CachedClasspathEntryKind::Jmod,
        };
        let path = entry.path().to_string_lossy().to_string();
        Self { kind, path }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
#[archive(check_bytes)]
struct EntryCacheArchive {
    fingerprint: ClasspathFingerprint,
    entry: CachedClasspathEntry,
    saved_at_millis: u64,
    stubs: Vec<ClasspathClassStub>,
}

pub fn load_or_build_entry<F>(
    cache_dir: &Path,
    entry: &ClasspathEntry,
    fingerprint: ClasspathFingerprint,
    target_release: Option<u16>,
    build: F,
) -> Result<Vec<ClasspathClassStub>, ClasspathError>
where
    F: FnOnce() -> Result<Vec<ClasspathClassStub>, ClasspathError>,
{
    let cache_path = cache_file_path(cache_dir, fingerprint, target_release);

    // Best-effort persistence: failures and corruption degrade to a cache miss.
    let cached_entry = CachedClasspathEntry::from_entry(entry);
    if let Some(stubs) = try_load(&cache_path, &cached_entry, fingerprint) {
        return Ok(stubs);
    }

    let stubs = build()?;

    // Best-effort write; do not fail indexing if the cache cannot be updated.
    let _ = try_store(cache_dir, &cache_path, &cached_entry, fingerprint, &stubs);

    Ok(stubs)
}

fn try_load(
    path: &Path,
    entry: &CachedClasspathEntry,
    fingerprint: ClasspathFingerprint,
) -> Option<Vec<ClasspathClassStub>> {
    // Avoid following symlinks out of the cache directory.
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            tracing::debug!(
                target = "nova.classpath",
                path = %path.display(),
                error = %err,
                "failed to stat classpath entry cache file"
            );
            return None;
        }
    };
    if meta.file_type().is_symlink() {
        remove_file_best_effort(path, "symlink");
        return None;
    }
    if !meta.is_file() {
        return None;
    }
    // If the file is absurdly large, treat it as corrupted and skip attempting to mmap it.
    let max_len =
        nova_storage::MAX_PAYLOAD_LEN_BYTES.saturating_add(nova_storage::HEADER_LEN as u64);
    if meta.len() > max_len {
        remove_file_best_effort(path, "oversize");
        return None;
    }

    let archive = match nova_storage::PersistedArchive::<EntryCacheArchive>::open_optional(
        path,
        nova_storage::ArtifactKind::ClasspathEntryStubs,
        CLASSPATH_ENTRY_CACHE_SCHEMA_VERSION,
    ) {
        Ok(Some(archive)) => archive,
        Ok(None) => return None,
        Err(err) => {
            tracing::debug!(
                target = "nova.classpath",
                path = %path.display(),
                error = %err,
                "failed to open classpath entry cache file"
            );
            remove_file_best_effort(path, "open_failed");
            return None;
        }
    };

    let value = match archive.to_owned() {
        Ok(value) => value,
        Err(err) => {
            tracing::debug!(
                target = "nova.classpath",
                path = %path.display(),
                error = %err,
                "failed to decode classpath entry cache file"
            );
            remove_file_best_effort(path, "decode_failed");
            return None;
        }
    };

    if value.fingerprint != fingerprint || &value.entry != entry {
        remove_file_best_effort(path, "key_mismatch");
        return None;
    }

    Some(value.stubs)
}

fn cache_file_path(
    cache_dir: &Path,
    fingerprint: ClasspathFingerprint,
    target_release: Option<u16>,
) -> PathBuf {
    match target_release {
        Some(r) => cache_dir.join(format!("classpath-entry-{}-r{r}.bin", fingerprint.to_hex())),
        None => cache_dir.join(format!("classpath-entry-{}.bin", fingerprint.to_hex())),
    }
}

fn try_store(
    cache_dir: &Path,
    path: &Path,
    entry: &CachedClasspathEntry,
    fingerprint: ClasspathFingerprint,
    stubs: &[ClasspathClassStub],
) -> Result<(), nova_storage::StorageError> {
    std::fs::create_dir_all(cache_dir)?;

    let persisted = EntryCacheArchive {
        fingerprint,
        entry: entry.clone(),
        saved_at_millis: now_millis(),
        stubs: stubs.to_vec(),
    };

    // Store compressed so `nova-storage` validates the content hash on load.
    nova_storage::write_archive_atomic(
        path,
        nova_storage::ArtifactKind::ClasspathEntryStubs,
        CLASSPATH_ENTRY_CACHE_SCHEMA_VERSION,
        &persisted,
        nova_storage::Compression::Zstd,
    )?;

    Ok(())
}

fn now_millis() -> u64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as u64,
        Err(err) => {
            // This should be extremely rare (system clock set before 1970). Avoid spamming logs
            // by reporting at most once.
            static REPORTED: OnceLock<()> = OnceLock::new();
            if REPORTED.set(()).is_ok() {
                tracing::debug!(
                    target = "nova.classpath",
                    error = %err,
                    "system time is before unix epoch; using 0 for now_millis"
                );
            }
            0
        }
    }
}

fn remove_file_best_effort(path: &Path, reason: &'static str) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            tracing::debug!(
                target = "nova.classpath",
                path = %path.display(),
                reason,
                error = %err,
                "failed to delete classpath entry cache file"
            );
        }
    }
}
