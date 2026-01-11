use nova_cache::{CacheConfig, CacheDir, ProjectSnapshot};
use nova_index::{
    load_index_view, save_indexes, AnnotationLocation, ProjectIndexes, ReferenceLocation,
    SymbolLocation,
};
use std::collections::BTreeSet;
use std::path::PathBuf;

#[test]
fn index_view_filters_invalidated_files_without_materializing() {
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
    // Put the same symbol/annotation in two files so invalidation can filter
    // out a subset of results.
    for file in ["A.java", "B.java"] {
        indexes.symbols.insert(
            "Foo",
            SymbolLocation {
                file: file.to_string(),
                line: 1,
                column: 1,
            },
        );
        indexes.references.insert(
            "Foo",
            ReferenceLocation {
                file: file.to_string(),
                line: 1,
                column: 1,
            },
        );
        indexes.annotations.insert(
            "@Deprecated",
            AnnotationLocation {
                file: file.to_string(),
                line: 1,
                column: 1,
            },
        );
    }

    save_indexes(&cache_dir, &snapshot_v1, &indexes).unwrap();

    let view_v1 = load_index_view(&cache_dir, &snapshot_v1).unwrap().unwrap();
    assert!(view_v1.invalidated_files.is_empty());
    assert_eq!(view_v1.symbol_locations("Foo").count(), 2);
    assert_eq!(view_v1.reference_locations("Foo").count(), 2);
    assert_eq!(view_v1.annotation_locations("@Deprecated").count(), 2);

    // Change a file so its fingerprint changes; A.java entries should be filtered.
    std::fs::write(&a, "class A { void m() {} }").unwrap();
    let snapshot_v2 = ProjectSnapshot::new(
        &project_root,
        vec![PathBuf::from("A.java"), PathBuf::from("B.java")],
    )
    .unwrap();

    let view_v2 = load_index_view(&cache_dir, &snapshot_v2).unwrap().unwrap();
    assert_eq!(
        view_v2.invalidated_files,
        BTreeSet::from(["A.java".to_string()])
    );

    // Ensure the query string doesn't have to outlive the iterator (the view
    // does not retain the lookup key).
    let symbol_iter = {
        let name = "Foo".to_string();
        view_v2.symbol_locations(&name)
    };
    let symbol_files: Vec<&str> = symbol_iter.map(|loc| loc.file.as_str()).collect();
    assert_eq!(symbol_files, vec!["B.java"]);

    let annotation_iter = {
        let name = "@Deprecated".to_string();
        view_v2.annotation_locations(&name)
    };
    let annotation_files: Vec<&str> = annotation_iter.map(|loc| loc.file.as_str()).collect();
    assert_eq!(annotation_files, vec!["B.java"]);

    let reference_iter = {
        let symbol = "Foo".to_string();
        view_v2.reference_locations(&symbol)
    };
    let reference_files: Vec<&str> = reference_iter.map(|loc| loc.file.as_str()).collect();
    assert_eq!(reference_files, vec!["B.java"]);
}
