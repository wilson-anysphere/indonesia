use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use bincode::Options;
use serde::{Deserialize, Serialize};

use crate::{ClasspathClassStub, ClasspathEntry, ClasspathError, ClasspathFingerprint};

const ENTRY_CACHE_MAGIC: [u8; 8] = *b"NOVACPTH";
const ENTRY_CACHE_FORMAT_VERSION: u32 = 1;
const CACHE_SCHEMA_VERSION: u32 = 1;

#[derive(Serialize)]
struct EntryCacheFile<'a> {
    magic: [u8; 8],
    cache_format_version: u32,
    schema_version: u32,
    nova_version: &'a str,
    fingerprint: ClasspathFingerprint,
    entry: &'a ClasspathEntry,
    stubs: &'a [ClasspathClassStub],
}

#[derive(Serialize, Deserialize)]
struct EntryCacheFileOwned {
    magic: [u8; 8],
    cache_format_version: u32,
    schema_version: u32,
    nova_version: String,
    fingerprint: ClasspathFingerprint,
    entry: ClasspathEntry,
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
    std::fs::create_dir_all(cache_dir)?;

    let cache_path = cache_file_path(cache_dir, fingerprint);
    let lock_path = cache_lock_file_path(&cache_path);
    let _lock = nova_cache::CacheLock::lock_exclusive(&lock_path)
        .map_err(|err| io::Error::other(err))?;

    if let Some(stubs) = try_load_entry_cache(&cache_path, entry, fingerprint) {
        return Ok(stubs);
    }

    let stubs = build()?;

    let file = EntryCacheFile {
        magic: ENTRY_CACHE_MAGIC,
        cache_format_version: ENTRY_CACHE_FORMAT_VERSION,
        schema_version: CACHE_SCHEMA_VERSION,
        nova_version: nova_core::NOVA_VERSION,
        fingerprint,
        entry,
        stubs: &stubs,
    };
    let bytes = bincode_options().serialize(&file)?;

    // Keep the on-disk payload size aligned with Nova's global bincode payload cap (ADR-0005).
    if bytes.len() <= nova_cache::BINCODE_PAYLOAD_LIMIT_BYTES {
        nova_cache::atomic_write(&cache_path, &bytes).map_err(|err| io::Error::other(err))?;
    }
    Ok(stubs)
}

fn cache_file_path(cache_dir: &Path, fingerprint: ClasspathFingerprint) -> PathBuf {
    cache_dir.join(format!("classpath-entry-{}.bin", fingerprint.to_hex()))
}

fn cache_lock_file_path(cache_file: &Path) -> PathBuf {
    cache_file.with_extension("lock")
}

fn bincode_options() -> impl bincode::Options + Copy {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
}

fn bincode_options_limited() -> impl bincode::Options + Copy {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .with_limit(nova_cache::BINCODE_PAYLOAD_LIMIT_BYTES as u64)
}

fn try_load_entry_cache(
    cache_path: &Path,
    entry: &ClasspathEntry,
    fingerprint: ClasspathFingerprint,
) -> Option<Vec<ClasspathClassStub>> {
    let bytes = read_cache_bytes_limited(cache_path)?;

    // Fast-path: if this isn't our cache file format, don't attempt to decode it.
    if !bytes.starts_with(&ENTRY_CACHE_MAGIC) {
        return None;
    }

    let file: EntryCacheFileOwned = match bincode_options_limited().deserialize(&bytes) {
        Ok(file) => file,
        Err(_) => {
            let _ = fs::remove_file(cache_path);
            return None;
        }
    };

    if file.magic != ENTRY_CACHE_MAGIC {
        return None;
    }
    if file.cache_format_version != ENTRY_CACHE_FORMAT_VERSION {
        return None;
    }
    if file.schema_version != CACHE_SCHEMA_VERSION {
        return None;
    }
    if file.nova_version != nova_core::NOVA_VERSION {
        return None;
    }
    if file.fingerprint != fingerprint {
        return None;
    }
    if file.entry != *entry {
        return None;
    }

    Some(file.stubs)
}

fn read_cache_bytes_limited(cache_path: &Path) -> Option<Vec<u8>> {
    // Avoid following symlinks out of the cache directory.
    let meta = fs::symlink_metadata(cache_path).ok()?;
    if meta.file_type().is_symlink() {
        let _ = fs::remove_file(cache_path);
        return None;
    }

    if !meta.is_file() {
        return None;
    }

    let limit = nova_cache::BINCODE_PAYLOAD_LIMIT_BYTES as u64;
    if meta.len() > limit {
        let _ = fs::remove_file(cache_path);
        return None;
    }

    let bytes = fs::read(cache_path).ok()?;
    if bytes.len() > nova_cache::BINCODE_PAYLOAD_LIMIT_BYTES {
        let _ = fs::remove_file(cache_path);
        return None;
    }

    Some(bytes)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tempfile::TempDir;

    use super::*;

    fn stub(name: &str) -> ClasspathClassStub {
        ClasspathClassStub {
            binary_name: name.to_string(),
            internal_name: name.replace('.', "/"),
            access_flags: 0,
            super_binary_name: None,
            interfaces: Vec::new(),
            signature: None,
            annotations: Vec::new(),
            fields: Vec::new(),
            methods: Vec::new(),
        }
    }

    #[test]
    fn entry_cache_hit_does_not_rebuild() {
        let tmp = TempDir::new().unwrap();
        let entry = ClasspathEntry::ClassDir(tmp.path().join("classes"));
        let fingerprint = ClasspathFingerprint(42);

        let calls = AtomicUsize::new(0);
        let first = load_or_build_entry(tmp.path(), &entry, fingerprint, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![stub("com.example.Foo")])
        })
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let second = load_or_build_entry(tmp.path(), &entry, fingerprint, || {
            panic!("expected cache hit, but builder was invoked")
        })
        .unwrap();

        assert_eq!(first, second);
    }

    #[test]
    fn garbled_cache_file_is_treated_as_miss_and_rebuilt() {
        let tmp = TempDir::new().unwrap();
        let entry = ClasspathEntry::ClassDir(tmp.path().join("classes"));
        let fingerprint = ClasspathFingerprint(7);

        let cache_path = cache_file_path(tmp.path(), fingerprint);
        fs::create_dir_all(tmp.path()).unwrap();
        fs::write(&cache_path, ENTRY_CACHE_MAGIC).unwrap();

        let calls = AtomicUsize::new(0);
        let first = load_or_build_entry(tmp.path(), &entry, fingerprint, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![stub("com.example.Bar")])
        })
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let second = load_or_build_entry(tmp.path(), &entry, fingerprint, || {
            panic!("expected cache hit after rebuild, but builder was invoked")
        })
        .unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn oversized_cache_file_is_treated_as_miss() {
        let tmp = TempDir::new().unwrap();
        let entry = ClasspathEntry::ClassDir(tmp.path().join("classes"));
        let fingerprint = ClasspathFingerprint(999);

        let cache_path = cache_file_path(tmp.path(), fingerprint);
        fs::create_dir_all(tmp.path()).unwrap();
        let file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .open(&cache_path)
            .unwrap();
        file.set_len(nova_cache::BINCODE_PAYLOAD_LIMIT_BYTES as u64 + 1)
            .unwrap();
        drop(file);

        let calls = AtomicUsize::new(0);
        let stubs = load_or_build_entry(tmp.path(), &entry, fingerprint, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![stub("com.example.Baz")])
        })
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(stubs, vec![stub("com.example.Baz")]);
    }

    #[test]
    fn nova_version_mismatch_is_treated_as_miss() {
        let tmp = TempDir::new().unwrap();
        let entry = ClasspathEntry::ClassDir(tmp.path().join("classes"));
        let fingerprint = ClasspathFingerprint(1234);

        let cache_path = cache_file_path(tmp.path(), fingerprint);
        let file = EntryCacheFileOwned {
            magic: ENTRY_CACHE_MAGIC,
            cache_format_version: ENTRY_CACHE_FORMAT_VERSION,
            schema_version: CACHE_SCHEMA_VERSION,
            nova_version: "definitely-not-the-current-version".to_string(),
            fingerprint,
            entry: entry.clone(),
            stubs: vec![stub("com.example.Legacy")],
        };
        let bytes = bincode_options().serialize(&file).unwrap();
        fs::write(&cache_path, bytes).unwrap();

        let calls = AtomicUsize::new(0);
        let first = load_or_build_entry(tmp.path(), &entry, fingerprint, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![stub("com.example.New")])
        })
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(first, vec![stub("com.example.New")]);

        let second = load_or_build_entry(tmp.path(), &entry, fingerprint, || {
            panic!("expected cache hit after nova version rebuild, but builder was invoked")
        })
        .unwrap();
        assert_eq!(second, vec![stub("com.example.New")]);
    }
}
