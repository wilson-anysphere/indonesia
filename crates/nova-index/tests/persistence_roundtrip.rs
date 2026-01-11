use nova_cache::{CacheConfig, CacheDir, CacheMetadata, ProjectSnapshot};
use nova_index::{
    load_indexes, load_indexes_fast, save_indexes, AnnotationLocation, InheritanceEdge,
    ProjectIndexes, ReferenceLocation, SymbolLocation,
};
use std::path::PathBuf;

#[test]
fn indexes_roundtrip_and_invalidation() {
    let temp = tempfile::tempdir().unwrap();
    let project_root = temp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();

    let a = project_root.join("A.java");
    let b = project_root.join("B.java");
    std::fs::write(&a, "class A {}").unwrap();
    std::fs::write(&b, "class B {}").unwrap();

    let snapshot_v1 = ProjectSnapshot::new(
        &project_root,
        vec![PathBuf::from("A.java"), PathBuf::from("B.java")],
    )
    .unwrap();

    let cache_dir = CacheDir::new(
        &project_root,
        CacheConfig {
            cache_root_override: Some(temp.path().join("cache-root")),
        },
    )
    .unwrap();

    let mut indexes = ProjectIndexes::default();
    indexes.symbols.insert(
        "A",
        SymbolLocation {
            file: "A.java".to_string(),
            line: 1,
            column: 1,
        },
    );
    indexes.references.insert(
        "A",
        ReferenceLocation {
            file: "B.java".to_string(),
            line: 1,
            column: 10,
        },
    );
    indexes.inheritance.insert(InheritanceEdge {
        file: "A.java".to_string(),
        subtype: "A".to_string(),
        supertype: "Object".to_string(),
    });
    indexes.inheritance.insert(InheritanceEdge {
        file: "B.java".to_string(),
        subtype: "B".to_string(),
        supertype: "Object".to_string(),
    });
    indexes.annotations.insert(
        "@Deprecated",
        AnnotationLocation {
            file: "A.java".to_string(),
            line: 1,
            column: 1,
        },
    );

    save_indexes(&cache_dir, &snapshot_v1, &mut indexes).unwrap();

    let loaded = load_indexes(&cache_dir, &snapshot_v1).unwrap().unwrap();
    assert!(loaded.invalidated_files.is_empty());
    assert_eq!(loaded.indexes, indexes);

    // Change a file so its fingerprint changes; A.java entries should be invalidated.
    std::fs::write(&a, "class A { void m() {} }").unwrap();
    let snapshot_v2 = ProjectSnapshot::new(
        &project_root,
        vec![PathBuf::from("A.java"), PathBuf::from("B.java")],
    )
    .unwrap();

    let loaded_v2 = load_indexes(&cache_dir, &snapshot_v2).unwrap().unwrap();
    assert_eq!(loaded_v2.invalidated_files, vec!["A.java".to_string()]);
    assert!(!loaded_v2.indexes.symbols.symbols.contains_key("A"));
    assert!(loaded_v2.indexes.references.references.contains_key("A"));
    assert!(loaded_v2
        .indexes
        .references
        .references
        .get("A")
        .unwrap()
        .iter()
        .all(|loc| loc.file != "A.java"));
    assert!(!loaded_v2.indexes.inheritance.supertypes.contains_key("A"));
    assert_eq!(
        loaded_v2
            .indexes
            .inheritance
            .supertypes
            .get("B")
            .unwrap()
            .as_slice(),
        &["Object".to_string()]
    );
    assert_eq!(
        loaded_v2
            .indexes
            .inheritance
            .subtypes
            .get("Object")
            .unwrap()
            .as_slice(),
        &["B".to_string()]
    );
    assert!(!loaded_v2
        .indexes
        .annotations
        .annotations
        .contains_key("@Deprecated"));
}

#[test]
fn indexes_invalidate_new_files() {
    let temp = tempfile::tempdir().unwrap();
    let project_root = temp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();

    let a = project_root.join("A.java");
    let b = project_root.join("B.java");
    std::fs::write(&a, "class A {}").unwrap();
    std::fs::write(&b, "class B {}").unwrap();

    let snapshot_v1 = ProjectSnapshot::new(
        &project_root,
        vec![PathBuf::from("A.java"), PathBuf::from("B.java")],
    )
    .unwrap();

    let cache_dir = CacheDir::new(
        &project_root,
        CacheConfig {
            cache_root_override: Some(temp.path().join("cache-root")),
        },
    )
    .unwrap();

    let mut indexes = ProjectIndexes::default();
    indexes.symbols.insert(
        "A",
        SymbolLocation {
            file: "A.java".to_string(),
            line: 1,
            column: 1,
        },
    );

    save_indexes(&cache_dir, &snapshot_v1, &mut indexes).unwrap();

    // Add a new file; it won't be present in the stored indexes, so it must be
    // marked as needing indexing on startup.
    let c = project_root.join("C.java");
    std::fs::write(&c, "class C {}").unwrap();
    let snapshot_v2 = ProjectSnapshot::new(
        &project_root,
        vec![
            PathBuf::from("A.java"),
            PathBuf::from("B.java"),
            PathBuf::from("C.java"),
        ],
    )
    .unwrap();

    let loaded_v2 = load_indexes(&cache_dir, &snapshot_v2).unwrap().unwrap();
    assert_eq!(loaded_v2.invalidated_files, vec!["C.java".to_string()]);
    assert_eq!(loaded_v2.indexes, indexes);
}

#[test]
fn indexes_invalidate_deleted_files() {
    let temp = tempfile::tempdir().unwrap();
    let project_root = temp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();

    let a = project_root.join("A.java");
    let b = project_root.join("B.java");
    std::fs::write(&a, "class A {}").unwrap();
    std::fs::write(&b, "class B {}").unwrap();

    let snapshot_v1 = ProjectSnapshot::new(
        &project_root,
        vec![PathBuf::from("A.java"), PathBuf::from("B.java")],
    )
    .unwrap();

    let cache_dir = CacheDir::new(
        &project_root,
        CacheConfig {
            cache_root_override: Some(temp.path().join("cache-root")),
        },
    )
    .unwrap();

    let mut indexes = ProjectIndexes::default();
    indexes.symbols.insert(
        "A",
        SymbolLocation {
            file: "A.java".to_string(),
            line: 1,
            column: 1,
        },
    );
    indexes.symbols.insert(
        "B",
        SymbolLocation {
            file: "B.java".to_string(),
            line: 1,
            column: 1,
        },
    );

    save_indexes(&cache_dir, &snapshot_v1, &mut indexes).unwrap();

    // Delete B.java; it must be invalidated and stripped from indexes.
    std::fs::remove_file(&b).unwrap();
    let snapshot_v2 = ProjectSnapshot::new(&project_root, vec![PathBuf::from("A.java")]).unwrap();

    let loaded_v2 = load_indexes(&cache_dir, &snapshot_v2).unwrap().unwrap();
    assert_eq!(loaded_v2.invalidated_files, vec!["B.java".to_string()]);
    assert_eq!(loaded_v2.indexes.symbols.symbols.len(), 1);
    assert!(loaded_v2.indexes.symbols.symbols.contains_key("A"));
    assert!(!loaded_v2.indexes.symbols.symbols.contains_key("B"));
}

#[test]
fn corrupt_metadata_is_cache_miss() {
    let temp = tempfile::tempdir().unwrap();
    let project_root = temp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();

    let a = project_root.join("A.java");
    std::fs::write(&a, "class A {}").unwrap();

    let snapshot = ProjectSnapshot::new(&project_root, vec![PathBuf::from("A.java")]).unwrap();

    let cache_dir = CacheDir::new(
        &project_root,
        CacheConfig {
            cache_root_override: Some(temp.path().join("cache-root")),
        },
    )
    .unwrap();

    let mut indexes = ProjectIndexes::default();
    indexes.symbols.insert(
        "A",
        SymbolLocation {
            file: "A.java".to_string(),
            line: 1,
            column: 1,
        },
    );

    save_indexes(&cache_dir, &snapshot, &mut indexes).unwrap();

    // Corrupt both the binary and JSON metadata so loading cannot fall back.
    let bin_path = cache_dir.metadata_bin_path();
    let bin = std::fs::OpenOptions::new()
        .write(true)
        .open(&bin_path)
        .unwrap();
    bin.set_len(1).unwrap();
    std::fs::write(cache_dir.metadata_path(), "this is not json").unwrap();

    let loaded = load_indexes(&cache_dir, &snapshot).unwrap();
    assert!(loaded.is_none());
}

#[test]
fn load_indexes_fast_detects_mtime_or_size_changes() {
    let temp = tempfile::tempdir().unwrap();
    let project_root = temp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();

    let a = project_root.join("A.java");
    let b = project_root.join("B.java");
    std::fs::write(&a, "class A {}").unwrap();
    std::fs::write(&b, "class B {}").unwrap();

    let snapshot_v1 = ProjectSnapshot::new(
        &project_root,
        vec![PathBuf::from("A.java"), PathBuf::from("B.java")],
    )
    .unwrap();

    let cache_dir = CacheDir::new(
        &project_root,
        CacheConfig {
            cache_root_override: Some(temp.path().join("cache-root")),
        },
    )
    .unwrap();

    let mut indexes = ProjectIndexes::default();
    indexes.symbols.insert(
        "A",
        SymbolLocation {
            file: "A.java".to_string(),
            line: 1,
            column: 1,
        },
    );
    save_indexes(&cache_dir, &snapshot_v1, &mut indexes).unwrap();

    // Modify A.java in a way that changes its size so the fast fingerprint must change even if
    // the filesystem has coarse mtime resolution.
    std::fs::write(&a, "class A { void m() {} }").unwrap();

    let loaded = load_indexes_fast(
        &cache_dir,
        &project_root,
        vec![PathBuf::from("A.java"), PathBuf::from("B.java")],
    )
    .unwrap()
    .unwrap();

    assert_eq!(loaded.invalidated_files, vec!["A.java".to_string()]);
}

#[test]
fn load_indexes_fast_does_not_read_project_file_contents() {
    let temp = tempfile::tempdir().unwrap();
    let project_root = temp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();

    let a = project_root.join("A.java");
    std::fs::write(&a, "class A {}").unwrap();

    let snapshot = ProjectSnapshot::new(&project_root, vec![PathBuf::from("A.java")]).unwrap();

    let cache_dir = CacheDir::new(
        &project_root,
        CacheConfig {
            cache_root_override: Some(temp.path().join("cache-root")),
        },
    )
    .unwrap();

    let mut indexes = ProjectIndexes::default();
    indexes.symbols.insert(
        "A",
        SymbolLocation {
            file: "A.java".to_string(),
            line: 1,
            column: 1,
        },
    );
    save_indexes(&cache_dir, &snapshot, &mut indexes).unwrap();

    // Replace the file with a directory. Reading contents would now fail, but metadata access
    // should still work.
    std::fs::remove_file(&a).unwrap();
    std::fs::create_dir_all(&a).unwrap();
    assert!(std::fs::read(&a).is_err());

    let loaded =
        load_indexes_fast(&cache_dir, &project_root, vec![PathBuf::from("A.java")]).unwrap();

    assert!(loaded.is_some());
}

#[test]
fn load_indexes_fast_schema_mismatch_is_cache_miss() {
    let temp = tempfile::tempdir().unwrap();
    let project_root = temp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();

    let a = project_root.join("A.java");
    std::fs::write(&a, "class A {}").unwrap();

    let snapshot = ProjectSnapshot::new(&project_root, vec![PathBuf::from("A.java")]).unwrap();

    let cache_dir = CacheDir::new(
        &project_root,
        CacheConfig {
            cache_root_override: Some(temp.path().join("cache-root")),
        },
    )
    .unwrap();

    let mut indexes = ProjectIndexes::default();
    indexes.symbols.insert(
        "A",
        SymbolLocation {
            file: "A.java".to_string(),
            line: 1,
            column: 1,
        },
    );
    save_indexes(&cache_dir, &snapshot, &mut indexes).unwrap();

    // Sanity check: the cache is readable through the fast path.
    assert!(
        load_indexes_fast(&cache_dir, &project_root, vec![PathBuf::from("A.java")])
            .unwrap()
            .is_some()
    );

    // Overwrite metadata.json with a v1 schema payload that lacks the new fast fingerprint field.
    let metadata = CacheMetadata::load(cache_dir.metadata_path()).unwrap();
    let file_fps = metadata
        .file_fingerprints
        .iter()
        .map(|(path, fp)| format!("\"{path}\":\"{}\"", fp.as_str()))
        .collect::<Vec<_>>()
        .join(",");
    let old_schema_json = format!(
        "{{\"schema_version\":1,\"nova_version\":\"{}\",\"created_at_millis\":{},\"last_updated_millis\":{},\"project_hash\":\"{}\",\"file_fingerprints\":{{{file_fps}}}}}",
        metadata.nova_version,
        metadata.created_at_millis,
        metadata.last_updated_millis,
        metadata.project_hash.as_str()
    );
    // Remove the binary metadata so the loader must fall back to JSON.
    std::fs::remove_file(cache_dir.metadata_bin_path()).unwrap();
    std::fs::write(cache_dir.metadata_path(), old_schema_json).unwrap();

    let loaded =
        load_indexes_fast(&cache_dir, &project_root, vec![PathBuf::from("A.java")]).unwrap();
    assert!(loaded.is_none());
}

#[test]
fn load_indexes_with_fingerprints_works_with_metadata_bin_only() {
    let temp = tempfile::tempdir().unwrap();
    let project_root = temp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();

    let a = project_root.join("A.java");
    std::fs::write(&a, "class A {}").unwrap();

    let snapshot = ProjectSnapshot::new(&project_root, vec![PathBuf::from("A.java")]).unwrap();

    let cache_dir = CacheDir::new(
        &project_root,
        CacheConfig {
            cache_root_override: Some(temp.path().join("cache-root")),
        },
    )
    .unwrap();

    let mut indexes = ProjectIndexes::default();
    indexes.symbols.insert(
        "A",
        SymbolLocation {
            file: "A.java".to_string(),
            line: 1,
            column: 1,
        },
    );

    save_indexes(&cache_dir, &snapshot, &mut indexes).unwrap();

    // Simulate an interruption between writing `metadata.bin` and `metadata.json`.
    std::fs::remove_file(cache_dir.metadata_path()).unwrap();
    assert!(cache_dir.metadata_bin_path().exists());

    let loaded = load_indexes_with_fingerprints(&cache_dir, snapshot.file_fingerprints())
        .unwrap()
        .unwrap();
    assert!(loaded.invalidated_files.is_empty());
}
