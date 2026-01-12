use std::path::{Path, PathBuf};
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
    build: F,
) -> Result<Vec<ClasspathClassStub>, ClasspathError>
where
    F: FnOnce() -> Result<Vec<ClasspathClassStub>, ClasspathError>,
{
    let cache_path = cache_file_path(cache_dir, fingerprint);

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
        Err(_) => return None,
    };
    if meta.file_type().is_symlink() {
        let _ = std::fs::remove_file(path);
        return None;
    }
    if !meta.is_file() {
        return None;
    }
    // If the file is absurdly large, treat it as corrupted and skip attempting to mmap it.
    let max_len = nova_storage::MAX_PAYLOAD_LEN_BYTES.saturating_add(nova_storage::HEADER_LEN as u64);
    if meta.len() > max_len {
        let _ = std::fs::remove_file(path);
        return None;
    }

    let archive = match nova_storage::PersistedArchive::<EntryCacheArchive>::open_optional(
        path,
        nova_storage::ArtifactKind::ClasspathEntryStubs,
        CLASSPATH_ENTRY_CACHE_SCHEMA_VERSION,
    ) {
        Ok(Some(archive)) => archive,
        Ok(None) => return None,
        Err(_) => {
            let _ = std::fs::remove_file(path);
            return None;
        }
    };

    let value = match archive.to_owned() {
        Ok(value) => value,
        Err(_) => {
            let _ = std::fs::remove_file(path);
            return None;
        }
    };

    if value.fingerprint != fingerprint || &value.entry != entry {
        let _ = std::fs::remove_file(path);
        return None;
    }

    Some(value.stubs)
}

fn cache_file_path(cache_dir: &Path, fingerprint: ClasspathFingerprint) -> PathBuf {
    cache_dir.join(format!("classpath-entry-{}.bin", fingerprint.to_hex()))
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
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
