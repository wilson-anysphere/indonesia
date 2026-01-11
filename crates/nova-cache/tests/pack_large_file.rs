use nova_cache::{pack_cache_package, CacheConfig, CacheDir, CacheMetadata, ProjectSnapshot};
use std::path::PathBuf;

#[test]
fn pack_cache_package_streams_large_index_files() -> Result<(), nova_cache::CacheError> {
    let tmp = tempfile::tempdir()?;
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(project_root.join("src"))?;
    std::fs::write(project_root.join("src/Main.java"), b"class Main {}")?;

    let cache_root = tmp.path().join("cache-root");
    let cache_dir = CacheDir::new(
        &project_root,
        CacheConfig {
            cache_root_override: Some(cache_root),
        },
    )?;

    let snapshot = ProjectSnapshot::new(&project_root, vec![PathBuf::from("src/Main.java")])?;
    let metadata = CacheMetadata::new(&snapshot);
    metadata.save(cache_dir.metadata_path())?;

    // Large, sparse index file: this previously required allocating a `Vec` with the full size
    // when computing checksums for the package manifest.
    let large_idx_path = cache_dir.indexes_dir().join("large.idx");
    let large_len = 128_u64 * 1024 * 1024;
    let file = std::fs::File::create(&large_idx_path)?;
    file.set_len(large_len)?;

    let package_path = tmp.path().join("cache-large.tar.zst");
    pack_cache_package(&cache_dir, &package_path)?;
    assert!(package_path.is_file());

    Ok(())
}

