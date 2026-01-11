use nova_cache::{CacheConfig, CacheDir, CacheMetadata, ProjectSnapshot};
use std::path::PathBuf;

#[test]
fn metadata_roundtrip() {
    let temp = tempfile::tempdir().unwrap();
    let project_root = temp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();

    let file_path = project_root.join("Main.java");
    std::fs::write(&file_path, "class Main {}").unwrap();

    let snapshot = ProjectSnapshot::new(&project_root, vec![PathBuf::from("Main.java")]).unwrap();

    let cache_dir = CacheDir::new(
        &project_root,
        CacheConfig {
            cache_root_override: Some(temp.path().join("cache-root")),
        },
    )
    .unwrap();

    let metadata = CacheMetadata::new(&snapshot);
    assert!(metadata
        .file_metadata_fingerprints
        .contains_key("Main.java"));
    metadata.save(cache_dir.metadata_path()).unwrap();

    let loaded = CacheMetadata::load(cache_dir.metadata_path()).unwrap();
    assert_eq!(metadata, loaded);
    loaded.ensure_compatible().unwrap();
}
