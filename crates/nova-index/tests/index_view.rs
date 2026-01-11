use nova_cache::{CacheConfig, CacheDir, CacheMetadata, ProjectSnapshot};
use nova_index::{
    load_index_view, load_index_view_fast, save_indexes, AnnotationLocation, ProjectIndexes,
    ReferenceLocation, SymbolLocation,
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
    indexes.symbols.insert(
        "OnlyA",
        SymbolLocation {
            file: "A.java".to_string(),
            line: 1,
            column: 1,
        },
    );
    indexes.references.insert(
        "OnlyA",
        ReferenceLocation {
            file: "A.java".to_string(),
            line: 1,
            column: 1,
        },
    );
    indexes.annotations.insert(
        "@OnlyA",
        AnnotationLocation {
            file: "A.java".to_string(),
            line: 1,
            column: 1,
        },
    );

    save_indexes(&cache_dir, &snapshot_v1, &indexes).unwrap();

    let view_v1 = load_index_view(&cache_dir, &snapshot_v1).unwrap().unwrap();
    assert!(view_v1.invalidated_files.is_empty());
    assert_eq!(view_v1.symbol_locations("Foo").count(), 2);
    assert_eq!(view_v1.symbol_locations("OnlyA").count(), 1);
    assert_eq!(view_v1.reference_locations("Foo").count(), 2);
    assert_eq!(view_v1.reference_locations("OnlyA").count(), 1);
    assert_eq!(view_v1.annotation_locations("@Deprecated").count(), 2);
    assert_eq!(view_v1.annotation_locations("@OnlyA").count(), 1);
    assert_eq!(
        view_v1.symbol_names().collect::<Vec<_>>(),
        vec!["Foo", "OnlyA"]
    );
    assert_eq!(
        view_v1.referenced_symbols().collect::<Vec<_>>(),
        vec!["Foo", "OnlyA"]
    );
    assert_eq!(
        view_v1.annotation_names().collect::<Vec<_>>(),
        vec!["@Deprecated", "@OnlyA"]
    );

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

    assert_eq!(view_v2.symbol_locations("OnlyA").count(), 0);
    assert_eq!(view_v2.reference_locations("OnlyA").count(), 0);
    assert_eq!(view_v2.annotation_locations("@OnlyA").count(), 0);

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

    assert_eq!(view_v2.symbol_names().collect::<Vec<_>>(), vec!["Foo"]);
    assert_eq!(view_v2.referenced_symbols().collect::<Vec<_>>(), vec!["Foo"]);
    assert_eq!(view_v2.annotation_names().collect::<Vec<_>>(), vec!["@Deprecated"]);
}

#[test]
fn index_view_filters_deleted_files() {
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
    for file in ["A.java", "B.java"] {
        indexes.symbols.insert(
            "Foo",
            SymbolLocation {
                file: file.to_string(),
                line: 1,
                column: 1,
            },
        );
    }
    save_indexes(&cache_dir, &snapshot_v1, &indexes).unwrap();

    // Delete B.java; it should be invalidated and filtered out of queries.
    std::fs::remove_file(&b).unwrap();
    let snapshot_v2 = ProjectSnapshot::new(&project_root, vec![PathBuf::from("A.java")]).unwrap();

    let view_v2 = load_index_view(&cache_dir, &snapshot_v2).unwrap().unwrap();
    assert_eq!(
        view_v2.invalidated_files,
        BTreeSet::from(["B.java".to_string()])
    );

    let files: Vec<&str> = view_v2
        .symbol_locations("Foo")
        .map(|loc| loc.file.as_str())
        .collect();
    assert_eq!(files, vec!["A.java"]);
}

#[test]
fn index_view_marks_new_files_invalidated_without_filtering_existing_results() {
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
    for file in ["A.java", "B.java"] {
        indexes.symbols.insert(
            "Foo",
            SymbolLocation {
                file: file.to_string(),
                line: 1,
                column: 1,
            },
        );
    }
    save_indexes(&cache_dir, &snapshot_v1, &indexes).unwrap();

    // Add C.java (new file): should be marked invalidated, but does not affect
    // queries into existing persisted results.
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

    let view_v2 = load_index_view(&cache_dir, &snapshot_v2).unwrap().unwrap();
    assert_eq!(
        view_v2.invalidated_files,
        BTreeSet::from(["C.java".to_string()])
    );

    let files: Vec<&str> = view_v2
        .symbol_locations("Foo")
        .map(|loc| loc.file.as_str())
        .collect();
    assert_eq!(files, vec!["A.java", "B.java"]);
}

#[test]
fn index_view_fast_filters_invalidated_files_without_materializing() {
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
    for file in ["A.java", "B.java"] {
        indexes.symbols.insert(
            "Foo",
            SymbolLocation {
                file: file.to_string(),
                line: 1,
                column: 1,
            },
        );
    }
    save_indexes(&cache_dir, &snapshot_v1, &indexes).unwrap();

    // Modify A.java in a way that changes its size so the fast fingerprint must change even if
    // the filesystem has coarse mtime resolution.
    std::fs::write(&a, "class A { void m() {} }").unwrap();

    let view = load_index_view_fast(
        &cache_dir,
        &project_root,
        vec![PathBuf::from("A.java"), PathBuf::from("B.java")],
    )
    .unwrap()
    .unwrap();

    assert_eq!(view.invalidated_files, BTreeSet::from(["A.java".to_string()]));

    let files: Vec<&str> = view
        .symbol_locations("Foo")
        .map(|loc| loc.file.as_str())
        .collect();
    assert_eq!(files, vec!["B.java"]);
}

#[test]
fn index_view_fast_does_not_read_project_file_contents() {
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
    save_indexes(&cache_dir, &snapshot, &indexes).unwrap();

    // Replace the file with a directory. Reading contents would now fail, but metadata access
    // should still work.
    std::fs::remove_file(&a).unwrap();
    std::fs::create_dir_all(&a).unwrap();
    assert!(std::fs::read(&a).is_err());

    let view =
        load_index_view_fast(&cache_dir, &project_root, vec![PathBuf::from("A.java")]).unwrap();
    assert!(view.is_some());
}

#[test]
fn index_view_fast_schema_mismatch_is_cache_miss() {
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
    save_indexes(&cache_dir, &snapshot, &indexes).unwrap();

    // Sanity check: the cache is readable through the fast path.
    assert!(
        load_index_view_fast(&cache_dir, &project_root, vec![PathBuf::from("A.java")])
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
    std::fs::write(cache_dir.metadata_path(), old_schema_json).unwrap();

    let view =
        load_index_view_fast(&cache_dir, &project_root, vec![PathBuf::from("A.java")]).unwrap();
    assert!(view.is_none());
}
