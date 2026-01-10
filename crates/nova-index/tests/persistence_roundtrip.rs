use nova_cache::{CacheConfig, CacheDir, ProjectSnapshot};
use nova_index::{
    load_indexes, save_indexes, AnnotationLocation, ProjectIndexes, ReferenceLocation,
    SymbolLocation, InheritanceEdge,
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

    save_indexes(&cache_dir, &snapshot_v1, &indexes).unwrap();

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
    assert!(loaded_v2
        .indexes
        .references
        .references
        .contains_key("A"));
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

    save_indexes(&cache_dir, &snapshot_v1, &indexes).unwrap();

    // Add a new file; it won't be present in the stored indexes, so it must be
    // marked as needing indexing on startup.
    let c = project_root.join("C.java");
    std::fs::write(&c, "class C {}").unwrap();
    let snapshot_v2 = ProjectSnapshot::new(
        &project_root,
        vec![PathBuf::from("A.java"), PathBuf::from("B.java"), PathBuf::from("C.java")],
    )
    .unwrap();

    let loaded_v2 = load_indexes(&cache_dir, &snapshot_v2).unwrap().unwrap();
    assert_eq!(loaded_v2.invalidated_files, vec!["C.java".to_string()]);
    assert_eq!(loaded_v2.indexes, indexes);
}
