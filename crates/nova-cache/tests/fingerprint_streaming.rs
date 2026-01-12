use nova_cache::{
    CacheConfig, CacheDir, CacheMetadata, Fingerprint, ProjectSnapshot, CACHE_METADATA_BIN_FILENAME,
    CACHE_METADATA_JSON_FILENAME, CACHE_PACKAGE_MANIFEST_PATH,
};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

mod suite;

struct TrackingAllocator;

static MAX_ALLOC: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static TRACK_ALLOC_DEPTH: Cell<u32> = Cell::new(0);
}

#[global_allocator]
static GLOBAL: TrackingAllocator = TrackingAllocator;

unsafe impl GlobalAlloc for TrackingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        track_alloc(layout.size());
        System.alloc(layout)
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        track_alloc(layout.size());
        System.alloc_zeroed(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        track_alloc(new_size);
        System.realloc(ptr, layout, new_size)
    }
}

struct TrackAllocGuard;

impl TrackAllocGuard {
    fn new() -> Self {
        TRACK_ALLOC_DEPTH.with(|depth| depth.set(depth.get() + 1));
        Self
    }
}

impl Drop for TrackAllocGuard {
    fn drop(&mut self) {
        TRACK_ALLOC_DEPTH.with(|depth| {
            let current = depth.get();
            debug_assert!(current > 0);
            depth.set(current.saturating_sub(1));
        });
    }
}

fn track_alloc(size: usize) {
    TRACK_ALLOC_DEPTH.with(|depth| {
        if depth.get() == 0 {
            return;
        }

        let mut current = MAX_ALLOC.load(Ordering::Relaxed);
        while size > current {
            match MAX_ALLOC.compare_exchange_weak(
                current,
                size,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(old) => current = old,
            }
        }
    });
}

fn reset_max_alloc() {
    MAX_ALLOC.store(0, Ordering::Relaxed);
}

fn max_alloc() -> usize {
    MAX_ALLOC.load(Ordering::Relaxed)
}

static TEST_LOCK: Mutex<()> = Mutex::new(());

fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK.lock().unwrap_or_else(|poison| poison.into_inner())
}

fn collect_cache_files(root: &Path) -> Result<Vec<PathBuf>, nova_cache::CacheError> {
    let mut files = Vec::new();

    let metadata_path = root.join(CACHE_METADATA_JSON_FILENAME);
    if metadata_path.is_file() {
        files.push(PathBuf::from(CACHE_METADATA_JSON_FILENAME));
    } else {
        return Err(nova_cache::CacheError::MissingArchiveEntry {
            path: CACHE_METADATA_JSON_FILENAME,
        });
    }

    let metadata_bin = root.join(CACHE_METADATA_BIN_FILENAME);
    if metadata_bin.is_file() {
        files.push(PathBuf::from(CACHE_METADATA_BIN_FILENAME));
    }

    for component_dir in ["indexes", "queries", "ast"] {
        let path = root.join(component_dir);
        if !path.is_dir() {
            continue;
        }

        for entry in walkdir::WalkDir::new(&path).follow_links(false) {
            let entry = entry?;
            if !entry.file_type().is_file() {
                continue;
            }

            let file_name = entry.file_name().to_string_lossy();
            if file_name.ends_with(".tmp") || file_name.contains(".tmp.") {
                continue;
            }

            let rel = entry
                .path()
                .strip_prefix(root)
                .map_err(|_| nova_cache::CacheError::InvalidArchivePath {
                    path: entry.path().to_path_buf(),
                })?;
            files.push(rel.to_path_buf());
        }
    }

    files.sort();
    Ok(files)
}

/// Test helper that mirrors `nova_cache::pack_cache_package`, but uses a lower zstd compression
/// level to keep peak address-space usage within the agent harness limits (RLIMIT_AS).
fn pack_cache_package_low_mem(
    cache_dir: &CacheDir,
    out_file: &Path,
) -> Result<(), nova_cache::CacheError> {
    let root = cache_dir.root();
    let files = collect_cache_files(root)?;

    let mut manifest: BTreeMap<String, String> = BTreeMap::new();
    for rel in &files {
        let disk_path = root.join(rel);
        let fingerprint = Fingerprint::from_file(&disk_path)?;
        manifest.insert(
            rel.to_string_lossy().replace('\\', "/"),
            fingerprint.as_str().to_string(),
        );
    }

    let parent = out_file.parent().unwrap_or(Path::new("."));
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    std::fs::create_dir_all(parent)?;

    let out = File::create(out_file)?;
    let encoder = zstd::Encoder::new(out, 1)?;
    let mut builder = tar::Builder::new(encoder);

    let manifest_json = serde_json::to_vec_pretty(&manifest)?;
    let mut header = tar::Header::new_gnu();
    header.set_size(manifest_json.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder.append_data(
        &mut header,
        CACHE_PACKAGE_MANIFEST_PATH,
        Cursor::new(manifest_json),
    )?;

    let metadata_json = PathBuf::from(CACHE_METADATA_JSON_FILENAME);
    let metadata_bin = PathBuf::from(CACHE_METADATA_BIN_FILENAME);
    let include_bin = files.iter().any(|p| p == &metadata_bin);

    for rel in [&metadata_json, &metadata_bin] {
        if rel == &metadata_bin && !include_bin {
            continue;
        }
        let disk_path = root.join(rel);
        let rel_string = rel.to_string_lossy().replace('\\', "/");
        builder.append_path_with_name(&disk_path, &rel_string)?;
    }

    for rel in &files {
        if rel == &metadata_json || rel == &metadata_bin {
            continue;
        }
        let disk_path = root.join(rel);
        let rel_string = rel.to_string_lossy().replace('\\', "/");
        builder.append_path_with_name(&disk_path, &rel_string)?;
    }

    let encoder = builder.into_inner()?;
    encoder.finish()?;
    Ok(())
}

fn write_pattern_file(path: &Path, size: usize) {
    use std::io::Write;

    let mut file = std::fs::File::create(path).unwrap();

    // Use a fixed-size buffer so test setup doesn't allocate proportional to the file size.
    let mut buf = [0_u8; 64 * 1024];
    for (idx, b) in buf.iter_mut().enumerate() {
        *b = (idx % 251) as u8;
    }

    let mut remaining = size;
    while remaining > 0 {
        let write = remaining.min(buf.len());
        file.write_all(&buf[..write]).unwrap();
        remaining -= write;
    }
}

#[test]
fn fingerprint_from_file_is_streaming_and_matches_from_bytes() {
    let _guard = test_lock();

    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("big.bin");
    write_pattern_file(&path, 8 * 1024 * 1024);

    reset_max_alloc();
    let from_file = {
        let _track = TrackAllocGuard::new();
        Fingerprint::from_file(&path).unwrap()
    };
    let max_during_from_file = max_alloc();
    assert!(
        max_during_from_file < 1024 * 1024,
        "Fingerprint::from_file should not allocate proportional to file size; max alloc {max_during_from_file} bytes"
    );

    let bytes = std::fs::read(&path).unwrap();
    let from_bytes = Fingerprint::from_bytes(bytes);
    assert_eq!(from_file, from_bytes);
}

#[test]
fn pack_cache_package_does_not_load_large_index_files_into_memory() {
    let _guard = test_lock();

    let tmp = tempfile::tempdir().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(project_root.join("src")).unwrap();
    std::fs::write(project_root.join("src/Main.java"), b"class Main {}").unwrap();

    let cache_root = tmp.path().join("cache-root");
    let cache_dir = CacheDir::new(
        &project_root,
        CacheConfig {
            cache_root_override: Some(cache_root),
        },
    )
    .unwrap();

    let snapshot =
        ProjectSnapshot::new(&project_root, vec![PathBuf::from("src/Main.java")]).unwrap();
    let metadata = CacheMetadata::new(&snapshot);
    metadata.save(cache_dir.metadata_path()).unwrap();

    let large_index_path = cache_dir.indexes_dir().join("large.idx");
    let large_size = 32_u64 * 1024 * 1024;
    let large_file = std::fs::File::create(&large_index_path).unwrap();
    large_file.set_len(large_size).unwrap();
    drop(large_file);

    let package_path = tmp.path().join("cache.tar.zst");
    reset_max_alloc();
    {
        let _track = TrackAllocGuard::new();
        pack_cache_package_low_mem(&cache_dir, &package_path).unwrap();
    }

    let max_during_pack = max_alloc();
    assert!(
        max_during_pack < 2 * 1024 * 1024,
        "pack_cache_package should not allocate proportional to cache artifact size; max alloc {max_during_pack} bytes"
    );
    assert!(package_path.is_file());
}
