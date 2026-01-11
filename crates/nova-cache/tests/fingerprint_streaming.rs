use nova_cache::{
    pack_cache_package, CacheConfig, CacheDir, CacheMetadata, Fingerprint, ProjectSnapshot,
};
use std::alloc::{GlobalAlloc, Layout, System};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

struct TrackingAllocator;

static MAX_ALLOC: AtomicUsize = AtomicUsize::new(0);

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

fn track_alloc(size: usize) {
    let mut current = MAX_ALLOC.load(Ordering::Relaxed);
    while size > current {
        match MAX_ALLOC.compare_exchange_weak(current, size, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(old) => current = old,
        }
    }
}

fn reset_max_alloc() {
    MAX_ALLOC.store(0, Ordering::Relaxed);
}

fn max_alloc() -> usize {
    MAX_ALLOC.load(Ordering::Relaxed)
}

static TEST_LOCK: Mutex<()> = Mutex::new(());

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
    let _guard = TEST_LOCK.lock().unwrap();

    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("big.bin");
    write_pattern_file(&path, 8 * 1024 * 1024);

    reset_max_alloc();
    let from_file = Fingerprint::from_file(&path).unwrap();
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
    let _guard = TEST_LOCK.lock().unwrap();

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
    pack_cache_package(&cache_dir, &package_path).unwrap();

    let max_during_pack = max_alloc();
    assert!(
        max_during_pack < 2 * 1024 * 1024,
        "pack_cache_package should not allocate proportional to cache artifact size; max alloc {max_during_pack} bytes"
    );
    assert!(package_path.is_file());
}
