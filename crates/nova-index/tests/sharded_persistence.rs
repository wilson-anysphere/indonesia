use nova_cache::{CacheConfig, CacheDir, ProjectSnapshot};
use nova_index::{
    affected_shards, load_sharded_index_archives, load_sharded_index_archives_fast,
    load_sharded_index_view, save_sharded_indexes, shard_id_for_path, AnnotationLocation,
    InheritanceEdge, ProjectIndexes, ReferenceLocation, SymbolLocation,
};
use std::collections::BTreeSet;
use std::path::PathBuf;

fn empty_shards(shard_count: u32) -> Vec<ProjectIndexes> {
    (0..shard_count)
        .map(|_| ProjectIndexes::default())
        .collect()
}

#[test]
fn sharded_roundtrip_loads_all_shards() {
    let shard_count = 16;

    let temp = tempfile::tempdir().unwrap();
    let project_root = temp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();

    let a = project_root.join("A.java");
    let b = project_root.join("B.java");
    std::fs::write(&a, "class A {}").unwrap();
    std::fs::write(&b, "class B {}").unwrap();

    let snapshot = ProjectSnapshot::new(
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

    let mut shards = empty_shards(shard_count);

    let shard_a = shard_id_for_path("A.java", shard_count) as usize;
    let shard_b = shard_id_for_path("B.java", shard_count) as usize;

    shards[shard_a].symbols.insert(
        "A",
        SymbolLocation {
            file: "A.java".to_string(),
            line: 1,
            column: 1,
        },
    );
    shards[shard_b].symbols.insert(
        "B",
        SymbolLocation {
            file: "B.java".to_string(),
            line: 1,
            column: 1,
        },
    );
    shards[shard_b].references.insert(
        "A",
        ReferenceLocation {
            file: "B.java".to_string(),
            line: 1,
            column: 10,
        },
    );
    shards[shard_a].inheritance.insert(InheritanceEdge {
        file: "A.java".to_string(),
        subtype: "A".to_string(),
        supertype: "Object".to_string(),
    });
    shards[shard_b].annotations.insert(
        "@Deprecated",
        AnnotationLocation {
            file: "B.java".to_string(),
            line: 1,
            column: 1,
        },
    );

    save_sharded_indexes(&cache_dir, &snapshot, shard_count, shards.clone()).unwrap();

    let loaded = load_sharded_index_archives(&cache_dir, &snapshot, shard_count)
        .unwrap()
        .unwrap();
    assert!(loaded.invalidated_files.is_empty());
    assert!(loaded.missing_shards.is_empty());
    assert_eq!(loaded.shards.len(), shard_count as usize);

    for (idx, shard_archives) in loaded.shards.into_iter().enumerate() {
        let shard_archives = shard_archives.expect("all shards should be present after save");
        let owned = ProjectIndexes {
            symbols: shard_archives.symbols.to_owned().unwrap(),
            references: shard_archives.references.to_owned().unwrap(),
            inheritance: shard_archives.inheritance.to_owned().unwrap(),
            annotations: shard_archives.annotations.to_owned().unwrap(),
        };
        assert_eq!(owned, shards[idx]);
    }

    let view = load_sharded_index_view(&cache_dir, &snapshot, shard_count)
        .unwrap()
        .unwrap();
    let a_locations = view.view.symbol_locations("A");
    assert_eq!(a_locations.len(), 1);
    assert_eq!(a_locations[0].file, "A.java");
}

#[test]
fn invalidated_files_map_to_affected_shards() {
    let shard_count = 16;

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

    let mut shards = empty_shards(shard_count);
    let shard_a = shard_id_for_path("A.java", shard_count) as usize;
    shards[shard_a].symbols.insert(
        "A",
        SymbolLocation {
            file: "A.java".to_string(),
            line: 1,
            column: 1,
        },
    );
    save_sharded_indexes(&cache_dir, &snapshot_v1, shard_count, shards).unwrap();

    // Change only A.java so only its shard is affected.
    std::fs::write(&a, "class A { void m() {} }").unwrap();
    let snapshot_v2 = ProjectSnapshot::new(
        &project_root,
        vec![PathBuf::from("A.java"), PathBuf::from("B.java")],
    )
    .unwrap();

    let loaded_v2 = load_sharded_index_archives(&cache_dir, &snapshot_v2, shard_count)
        .unwrap()
        .unwrap();

    assert_eq!(loaded_v2.invalidated_files, vec!["A.java".to_string()]);

    let affected = affected_shards(&loaded_v2.invalidated_files, shard_count);
    let expected: BTreeSet<_> = [shard_id_for_path("A.java", shard_count)]
        .into_iter()
        .collect();
    assert_eq!(affected, expected);
}

#[test]
fn sharded_index_view_filters_invalidated_files() {
    let shard_count = 16;

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

    let mut shards = empty_shards(shard_count);
    shards[shard_id_for_path("A.java", shard_count) as usize]
        .symbols
        .insert(
            "A",
            SymbolLocation {
                file: "A.java".to_string(),
                line: 1,
                column: 1,
            },
        );
    shards[shard_id_for_path("B.java", shard_count) as usize]
        .symbols
        .insert(
            "B",
            SymbolLocation {
                file: "B.java".to_string(),
                line: 1,
                column: 1,
            },
        );
    save_sharded_indexes(&cache_dir, &snapshot_v1, shard_count, shards).unwrap();

    // Modify A.java so it is marked invalidated, but keep shard files intact on disk.
    std::fs::write(&a, "class A { void m() {} }").unwrap();
    let snapshot_v2 = ProjectSnapshot::new(
        &project_root,
        vec![PathBuf::from("A.java"), PathBuf::from("B.java")],
    )
    .unwrap();

    let loaded_v2 = load_sharded_index_view(&cache_dir, &snapshot_v2, shard_count)
        .unwrap()
        .unwrap();
    assert_eq!(loaded_v2.invalidated_files, vec!["A.java".to_string()]);

    // The view should hide stale locations from invalidated files without requiring deserialization.
    assert!(loaded_v2.view.symbol_locations("A").is_empty());
    assert_eq!(loaded_v2.view.symbol_locations("B").len(), 1);
}

#[test]
fn corrupt_shard_is_partial_cache_miss() {
    let shard_count = 64;

    let temp = tempfile::tempdir().unwrap();
    let project_root = temp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();

    // Pick two file names that land in different shards to ensure we can observe partial failure.
    let mut paths = Vec::new();
    let mut seen = BTreeSet::new();
    for idx in 0..500u32 {
        let name = format!("File{idx}.java");
        let shard_id = shard_id_for_path(&name, shard_count);
        if seen.insert(shard_id) {
            paths.push(name);
        }
        if paths.len() >= 2 {
            break;
        }
    }
    assert_eq!(paths.len(), 2);

    for name in &paths {
        std::fs::write(project_root.join(name), format!("class {} {{}}", name)).unwrap();
    }

    let snapshot =
        ProjectSnapshot::new(&project_root, paths.iter().map(PathBuf::from).collect()).unwrap();

    let cache_dir = CacheDir::new(
        &project_root,
        CacheConfig {
            cache_root_override: Some(temp.path().join("cache-root")),
        },
    )
    .unwrap();

    let mut shards = empty_shards(shard_count);
    for name in &paths {
        let shard = shard_id_for_path(name, shard_count) as usize;
        let symbol = name.trim_end_matches(".java");
        shards[shard].symbols.insert(
            symbol,
            SymbolLocation {
                file: name.clone(),
                line: 1,
                column: 1,
            },
        );
    }
    save_sharded_indexes(&cache_dir, &snapshot, shard_count, shards).unwrap();

    // Corrupt one shard file.
    let corrupt_shard = shard_id_for_path(&paths[0], shard_count);
    let corrupt_path = cache_dir
        .indexes_dir()
        .join("shards")
        .join(corrupt_shard.to_string())
        .join("symbols.idx");
    std::fs::write(&corrupt_path, b"corrupt").unwrap();

    let loaded = load_sharded_index_archives(&cache_dir, &snapshot, shard_count)
        .unwrap()
        .unwrap();

    assert!(loaded.missing_shards.contains(&corrupt_shard));
    assert!(loaded.shards[corrupt_shard as usize].is_none());

    let other_shard = shard_id_for_path(&paths[1], shard_count);
    assert!(loaded.shards[other_shard as usize].is_some());

    let expected_invalidated: BTreeSet<String> = paths
        .iter()
        .filter(|path| shard_id_for_path(path, shard_count) == corrupt_shard)
        .cloned()
        .collect();
    assert_eq!(
        loaded
            .invalidated_files
            .into_iter()
            .collect::<BTreeSet<_>>(),
        expected_invalidated
    );
}

#[test]
fn sharded_save_rewrites_all_shards_when_shard_count_changes() {
    let shard_count_v1 = 16;
    let shard_count_v2 = 32;

    let temp = tempfile::tempdir().unwrap();
    let project_root = temp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();

    let file = (0..1000u32)
        .map(|idx| format!("File{idx}.java"))
        .find(|name| shard_id_for_path(name, shard_count_v2) >= shard_count_v1)
        .expect("expected to find a filename that moves shards when shard_count changes");

    let full_path = project_root.join(&file);
    std::fs::write(&full_path, "class A {}").unwrap();

    let snapshot = ProjectSnapshot::new(&project_root, vec![PathBuf::from(&file)]).unwrap();
    let cache_dir = CacheDir::new(
        &project_root,
        CacheConfig {
            cache_root_override: Some(temp.path().join("cache-root")),
        },
    )
    .unwrap();

    let mut shards_v1 = empty_shards(shard_count_v1);
    shards_v1[shard_id_for_path(&file, shard_count_v1) as usize]
        .symbols
        .insert(
            "A",
            SymbolLocation {
                file: file.clone(),
                line: 1,
                column: 1,
            },
        );
    save_sharded_indexes(&cache_dir, &snapshot, shard_count_v1, shards_v1).unwrap();

    let mut shards_v2 = empty_shards(shard_count_v2);
    shards_v2[shard_id_for_path(&file, shard_count_v2) as usize]
        .symbols
        .insert(
            "A",
            SymbolLocation {
                file: file.clone(),
                line: 1,
                column: 1,
            },
        );
    save_sharded_indexes(&cache_dir, &snapshot, shard_count_v2, shards_v2.clone()).unwrap();

    let loaded = load_sharded_index_archives(&cache_dir, &snapshot, shard_count_v2)
        .unwrap()
        .unwrap();
    assert!(loaded.missing_shards.is_empty());
    assert!(loaded.invalidated_files.is_empty());

    for (idx, shard_archives) in loaded.shards.into_iter().enumerate() {
        let shard_archives = shard_archives.expect("all shards present after full rewrite");
        let owned = ProjectIndexes {
            symbols: shard_archives.symbols.to_owned().unwrap(),
            references: shard_archives.references.to_owned().unwrap(),
            inheritance: shard_archives.inheritance.to_owned().unwrap(),
            annotations: shard_archives.annotations.to_owned().unwrap(),
        };
        assert_eq!(owned, shards_v2[idx]);
    }
}

#[test]
fn sharded_fast_load_does_not_read_project_file_contents() {
    let shard_count = 16;

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

    let mut shards = empty_shards(shard_count);
    let shard_a = shard_id_for_path("A.java", shard_count) as usize;
    shards[shard_a].symbols.insert(
        "A",
        SymbolLocation {
            file: "A.java".to_string(),
            line: 1,
            column: 1,
        },
    );
    save_sharded_indexes(&cache_dir, &snapshot, shard_count, shards).unwrap();

    // Replace the file with a directory. Reading contents would now fail, but metadata access
    // should still work.
    std::fs::remove_file(&a).unwrap();
    std::fs::create_dir_all(&a).unwrap();
    assert!(std::fs::read(&a).is_err());

    let loaded = load_sharded_index_archives_fast(
        &cache_dir,
        &project_root,
        vec![PathBuf::from("A.java")],
        shard_count,
    )
    .unwrap();

    assert!(loaded.is_some());
}
