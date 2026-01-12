use nova_cache::{CacheConfig, CacheDir, ProjectSnapshot};
use nova_index::{
    affected_shards, load_sharded_index_archives, load_sharded_index_archives_fast,
    load_sharded_index_view, load_sharded_index_view_lazy, load_sharded_index_view_lazy_fast,
    save_sharded_indexes, save_sharded_indexes_incremental, shard_id_for_path, AnnotationLocation,
    IndexSymbolKind, IndexedSymbol, InheritanceEdge, ProjectIndexes, ReferenceLocation,
    ShardedIndexOverlay, SymbolLocation,
};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

fn empty_shards(shard_count: u32) -> Vec<ProjectIndexes> {
    (0..shard_count)
        .map(|_| ProjectIndexes::default())
        .collect()
}

fn assert_send_sync<T: Send + Sync>() {}

fn sym(name: &str, file: &str, line: u32, column: u32) -> IndexedSymbol {
    IndexedSymbol {
        qualified_name: name.to_string(),
        kind: IndexSymbolKind::Class,
        container_name: None,
        location: SymbolLocation {
            file: file.to_string(),
            line,
            column,
        },
        ast_id: 0,
    }
}

#[test]
fn lazy_sharded_index_view_is_send_sync() {
    assert_send_sync::<nova_index::LazyShardedIndexView>();
}

#[test]
fn lazy_sharded_load_is_cache_miss_when_symbols_idx_schema_mismatches() {
    use std::io::{Seek, SeekFrom, Write};

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
    shards[shard_a]
        .symbols
        .insert("A", sym("A", "A.java", 1, 1));

    save_sharded_indexes(&cache_dir, &snapshot, shard_count, &mut shards).unwrap();

    let loaded = load_sharded_index_view_lazy(&cache_dir, &snapshot, shard_count)
        .unwrap()
        .expect("expected lazy shard cache hit");
    assert!(
        loaded.invalidated_files.is_empty(),
        "expected no invalidated files after save"
    );

    // Corrupt the persisted schema version in shard 0's symbols index header.
    let symbols_idx = cache_dir
        .indexes_dir()
        .join("shards")
        .join("0")
        .join("symbols.idx");
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&symbols_idx)
        .expect("open symbols.idx");
    // Layout: MAGIC(8) + header_version(u16) + kind(u16) + schema_version(u32).
    file.seek(SeekFrom::Start(12)).expect("seek schema_version");
    file.write_all(&(nova_index::INDEX_SCHEMA_VERSION + 1).to_le_bytes())
        .expect("overwrite schema_version");

    // `load_sharded_index_view_lazy` is allowed to avoid touching shard payloads on a cache hit,
    // but it should still treat schema mismatches as a cache miss.
    let loaded = load_sharded_index_view_lazy(&cache_dir, &snapshot, shard_count).unwrap();
    assert!(
        loaded.is_none(),
        "expected schema mismatch to force a cache miss"
    );
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

    shards[shard_a]
        .symbols
        .insert("A", sym("A", "A.java", 1, 1));
    shards[shard_b]
        .symbols
        .insert("B", sym("B", "B.java", 1, 1));
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

    save_sharded_indexes(&cache_dir, &snapshot, shard_count, &mut shards).unwrap();

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
    let a_locations: Vec<_> = view.view.symbol_locations("A").collect();
    assert_eq!(a_locations.len(), 1);
    assert_eq!(a_locations[0].file, "A.java");

    assert_eq!(view.view.symbol_names().collect::<Vec<_>>(), vec!["A", "B"]);
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
    shards[shard_a]
        .symbols
        .insert("A", sym("A", "A.java", 1, 1));
    save_sharded_indexes(&cache_dir, &snapshot_v1, shard_count, &mut shards).unwrap();

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
        .insert("A", sym("A", "A.java", 1, 1));
    shards[shard_id_for_path("B.java", shard_count) as usize]
        .symbols
        .insert("B", sym("B", "B.java", 1, 1));
    save_sharded_indexes(&cache_dir, &snapshot_v1, shard_count, &mut shards).unwrap();

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
    assert!(loaded_v2.view.symbol_locations("A").next().is_none());
    assert_eq!(loaded_v2.view.symbol_locations("B").count(), 1);
    assert_eq!(loaded_v2.view.symbol_names().collect::<Vec<_>>(), vec!["B"]);
}

#[test]
fn sharded_index_view_overlay_merges_invalidated_files() {
    let shard_count = 64;

    let temp = tempfile::tempdir().unwrap();
    let project_root = temp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();

    let file_a = "A.java".to_string();
    let shard_a = shard_id_for_path(&file_a, shard_count);
    let file_b = (0..500u32)
        .map(|idx| format!("B{idx}.java"))
        .find(|name| shard_id_for_path(name, shard_count) != shard_a)
        .expect("expected to find a filename in a different shard");

    let a = project_root.join(&file_a);
    let b = project_root.join(&file_b);
    std::fs::write(&a, "class A {}").unwrap();
    std::fs::write(&b, "class B {}").unwrap();

    let snapshot_v1 = ProjectSnapshot::new(
        &project_root,
        vec![PathBuf::from(&file_a), PathBuf::from(&file_b)],
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
    for file in [&file_a, &file_b] {
        let shard = shard_id_for_path(file, shard_count) as usize;
        shards[shard].symbols.insert("Foo", sym("Foo", file, 1, 1));
        shards[shard].references.insert(
            "Foo",
            ReferenceLocation {
                file: file.to_string(),
                line: 1,
                column: 1,
            },
        );
        shards[shard].annotations.insert(
            "@Deprecated",
            AnnotationLocation {
                file: file.to_string(),
                line: 1,
                column: 1,
            },
        );
    }

    let shard_a_idx = shard_id_for_path(&file_a, shard_count) as usize;
    shards[shard_a_idx]
        .symbols
        .insert("OnlyA", sym("OnlyA", &file_a, 1, 1));
    shards[shard_a_idx].references.insert(
        "OnlyA",
        ReferenceLocation {
            file: file_a.clone(),
            line: 1,
            column: 1,
        },
    );
    shards[shard_a_idx].annotations.insert(
        "@OnlyA",
        AnnotationLocation {
            file: file_a.clone(),
            line: 1,
            column: 1,
        },
    );

    save_sharded_indexes(&cache_dir, &snapshot_v1, shard_count, &mut shards).unwrap();

    // Modify A.java so it's invalidated.
    std::fs::write(&a, "class A { void m() {} }").unwrap();
    let snapshot_v2 = ProjectSnapshot::new(
        &project_root,
        vec![PathBuf::from(&file_a), PathBuf::from(&file_b)],
    )
    .unwrap();

    let mut loaded_v2 = load_sharded_index_view(&cache_dir, &snapshot_v2, shard_count)
        .unwrap()
        .unwrap();
    assert_eq!(loaded_v2.invalidated_files, vec![file_a.clone()]);

    // Persisted view should filter out A.java results.
    assert_eq!(
        loaded_v2
            .view
            .symbol_locations("Foo")
            .map(|loc| loc.file)
            .collect::<Vec<_>>(),
        vec![file_b.as_str()]
    );
    assert_eq!(loaded_v2.view.symbol_locations("OnlyA").count(), 0);
    assert_eq!(
        loaded_v2.view.symbol_names().collect::<Vec<_>>(),
        vec!["Foo"]
    );

    // Apply overlay deltas for the invalidated file.
    let mut delta = ProjectIndexes::default();
    for (symbol, column) in [("Foo", 1u32), ("OnlyA", 1), ("NewInA", 10)] {
        delta
            .symbols
            .insert(symbol, sym(symbol, &file_a, 2, column));
        delta.references.insert(
            symbol,
            ReferenceLocation {
                file: file_a.clone(),
                line: 2,
                column,
            },
        );
    }
    for (annotation, column) in [("@Deprecated", 1u32), ("@OnlyA", 1), ("@NewInA", 10)] {
        delta.annotations.insert(
            annotation,
            AnnotationLocation {
                file: file_a.clone(),
                line: 2,
                column,
            },
        );
    }

    loaded_v2.view.overlay.apply_file_delta(&file_a, delta);
    assert_eq!(
        loaded_v2.view.overlay.covered_files,
        BTreeSet::from([file_a.clone()])
    );

    assert_eq!(
        loaded_v2
            .view
            .symbol_locations_merged("Foo")
            .map(|loc| loc.file)
            .collect::<Vec<_>>(),
        vec![file_b.as_str(), file_a.as_str()]
    );
    assert_eq!(
        loaded_v2
            .view
            .reference_locations_merged("NewInA")
            .map(|loc| loc.file)
            .collect::<Vec<_>>(),
        vec![file_a.as_str()]
    );
    assert_eq!(
        loaded_v2
            .view
            .annotation_locations_merged("@Deprecated")
            .map(|loc| loc.file)
            .collect::<Vec<_>>(),
        vec![file_b.as_str(), file_a.as_str()]
    );

    assert_eq!(
        loaded_v2.view.symbol_names_merged().collect::<Vec<_>>(),
        vec!["Foo", "NewInA", "OnlyA"]
    );
    assert_eq!(
        loaded_v2
            .view
            .referenced_symbols_merged()
            .collect::<Vec<_>>(),
        vec!["Foo", "NewInA", "OnlyA"]
    );
    assert_eq!(
        loaded_v2.view.annotation_names_merged().collect::<Vec<_>>(),
        vec!["@Deprecated", "@NewInA", "@OnlyA"]
    );
    assert_eq!(
        loaded_v2
            .view
            .symbol_names_with_prefix_merged("N")
            .collect::<Vec<_>>(),
        vec!["NewInA"]
    );
    assert_eq!(
        loaded_v2
            .view
            .annotation_names_with_prefix_merged("@N")
            .collect::<Vec<_>>(),
        vec!["@NewInA"]
    );
    assert_eq!(
        loaded_v2
            .view
            .referenced_symbols_with_prefix_merged("O")
            .collect::<Vec<_>>(),
        vec!["OnlyA"]
    );
}

#[test]
fn lazy_sharded_index_view_overlay_merges_invalidated_files() {
    let shard_count = 64;

    let temp = tempfile::tempdir().unwrap();
    let project_root = temp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();

    let file_a = "A.java".to_string();
    let shard_a = shard_id_for_path(&file_a, shard_count);
    let file_b = (0..500u32)
        .map(|idx| format!("B{idx}.java"))
        .find(|name| shard_id_for_path(name, shard_count) != shard_a)
        .expect("expected to find a filename in a different shard");

    let a = project_root.join(&file_a);
    let b = project_root.join(&file_b);
    std::fs::write(&a, "class A {}").unwrap();
    std::fs::write(&b, "class B {}").unwrap();

    let snapshot_v1 = ProjectSnapshot::new(
        &project_root,
        vec![PathBuf::from(&file_a), PathBuf::from(&file_b)],
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
    for file in [&file_a, &file_b] {
        let shard = shard_id_for_path(file, shard_count) as usize;
        shards[shard].symbols.insert("Foo", sym("Foo", file, 1, 1));
        shards[shard].references.insert(
            "Foo",
            ReferenceLocation {
                file: file.to_string(),
                line: 1,
                column: 1,
            },
        );
        shards[shard].annotations.insert(
            "@Deprecated",
            AnnotationLocation {
                file: file.to_string(),
                line: 1,
                column: 1,
            },
        );
    }

    let shard_a_idx = shard_id_for_path(&file_a, shard_count) as usize;
    shards[shard_a_idx]
        .symbols
        .insert("OnlyA", sym("OnlyA", &file_a, 1, 1));
    shards[shard_a_idx].references.insert(
        "OnlyA",
        ReferenceLocation {
            file: file_a.clone(),
            line: 1,
            column: 1,
        },
    );
    shards[shard_a_idx].annotations.insert(
        "@OnlyA",
        AnnotationLocation {
            file: file_a.clone(),
            line: 1,
            column: 1,
        },
    );

    save_sharded_indexes(&cache_dir, &snapshot_v1, shard_count, &mut shards).unwrap();

    // Modify A.java so it's invalidated.
    std::fs::write(&a, "class A { void m() {} }").unwrap();
    let snapshot_v2 = ProjectSnapshot::new(
        &project_root,
        vec![PathBuf::from(&file_a), PathBuf::from(&file_b)],
    )
    .unwrap();

    let mut loaded_v2 = load_sharded_index_view_lazy(&cache_dir, &snapshot_v2, shard_count)
        .unwrap()
        .unwrap();
    assert_eq!(loaded_v2.invalidated_files, vec![file_a.clone()]);

    // Persisted view should filter out A.java results.
    assert_eq!(
        loaded_v2
            .view
            .symbol_locations("Foo")
            .map(|loc| loc.file)
            .collect::<Vec<_>>(),
        vec![file_b.as_str()]
    );
    assert_eq!(loaded_v2.view.symbol_locations("OnlyA").count(), 0);
    assert_eq!(
        loaded_v2.view.symbol_names().collect::<Vec<_>>(),
        vec!["Foo"]
    );

    // Apply overlay deltas for the invalidated file.
    let mut delta = ProjectIndexes::default();
    for (symbol, column) in [("Foo", 1u32), ("OnlyA", 1), ("NewInA", 10)] {
        delta
            .symbols
            .insert(symbol, sym(symbol, &file_a, 2, column));
        delta.references.insert(
            symbol,
            ReferenceLocation {
                file: file_a.clone(),
                line: 2,
                column,
            },
        );
    }
    for (annotation, column) in [("@Deprecated", 1u32), ("@OnlyA", 1), ("@NewInA", 10)] {
        delta.annotations.insert(
            annotation,
            AnnotationLocation {
                file: file_a.clone(),
                line: 2,
                column,
            },
        );
    }

    loaded_v2.view.overlay.apply_file_delta(&file_a, delta);
    assert_eq!(
        loaded_v2.view.overlay.covered_files,
        BTreeSet::from([file_a.clone()])
    );

    assert_eq!(
        loaded_v2
            .view
            .symbol_locations_merged("Foo")
            .map(|loc| loc.file)
            .collect::<Vec<_>>(),
        vec![file_b.as_str(), file_a.as_str()]
    );
    assert_eq!(
        loaded_v2
            .view
            .reference_locations_merged("NewInA")
            .map(|loc| loc.file)
            .collect::<Vec<_>>(),
        vec![file_a.as_str()]
    );
    assert_eq!(
        loaded_v2
            .view
            .annotation_locations_merged("@Deprecated")
            .map(|loc| loc.file)
            .collect::<Vec<_>>(),
        vec![file_b.as_str(), file_a.as_str()]
    );

    assert_eq!(
        loaded_v2.view.symbol_names_merged().collect::<Vec<_>>(),
        vec!["Foo", "NewInA", "OnlyA"]
    );
    assert_eq!(
        loaded_v2
            .view
            .referenced_symbols_merged()
            .collect::<Vec<_>>(),
        vec!["Foo", "NewInA", "OnlyA"]
    );
    assert_eq!(
        loaded_v2.view.annotation_names_merged().collect::<Vec<_>>(),
        vec!["@Deprecated", "@NewInA", "@OnlyA"]
    );
    assert_eq!(
        loaded_v2
            .view
            .symbol_names_with_prefix_merged("N")
            .collect::<Vec<_>>(),
        vec!["NewInA"]
    );
    assert_eq!(
        loaded_v2
            .view
            .annotation_names_with_prefix_merged("@N")
            .collect::<Vec<_>>(),
        vec!["@NewInA"]
    );
    assert_eq!(
        loaded_v2
            .view
            .referenced_symbols_with_prefix_merged("O")
            .collect::<Vec<_>>(),
        vec!["OnlyA"]
    );
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
        shards[shard]
            .symbols
            .insert(symbol, sym(symbol, name, 1, 1));
    }
    save_sharded_indexes(&cache_dir, &snapshot, shard_count, &mut shards).unwrap();

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
fn lazy_sharded_index_view_discovers_corrupt_shards_on_access() {
    let shard_count = 64;

    let temp = tempfile::tempdir().unwrap();
    let project_root = temp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();

    // Pick two file names that land in different shards so we can corrupt one shard without
    // affecting queries for the other.
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
        shards[shard]
            .symbols
            .insert(symbol, sym(symbol, name, 1, 1));
    }
    save_sharded_indexes(&cache_dir, &snapshot, shard_count, &mut shards).unwrap();

    // Corrupt one shard file.
    let corrupt_shard = shard_id_for_path(&paths[0], shard_count);
    let corrupt_path = cache_dir
        .indexes_dir()
        .join("shards")
        .join(corrupt_shard.to_string())
        .join("symbols.idx");
    std::fs::write(&corrupt_path, b"corrupt").unwrap();

    // The lazy loader should still succeed (it doesn't open shards up-front).
    let loaded = load_sharded_index_view_lazy(&cache_dir, &snapshot, shard_count)
        .unwrap()
        .unwrap();
    assert!(loaded.invalidated_files.is_empty());
    assert!(loaded.view.missing_shards().is_empty());

    // Accessing a healthy shard should work without discovering the corrupt shard.
    let healthy_shard = shard_id_for_path(&paths[1], shard_count);
    let healthy_symbol = paths[1].trim_end_matches(".java");
    let shard = loaded
        .view
        .shard(healthy_shard)
        .expect("healthy shard should load");
    let locations = shard
        .symbols
        .archived()
        .symbols
        .get(healthy_symbol)
        .expect("expected symbol to be present");
    assert_eq!(locations.iter().count(), 1);
    assert_eq!(
        locations.iter().next().unwrap().location.file.as_str(),
        paths[1].as_str()
    );
    assert!(loaded.view.missing_shards().is_empty());

    // Accessing the corrupt shard should treat it as missing (no panic).
    assert!(loaded.view.shard(corrupt_shard).is_none());
    assert!(loaded.view.missing_shards().contains(&corrupt_shard));
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
        .insert("A", sym("A", &file, 1, 1));
    save_sharded_indexes(&cache_dir, &snapshot, shard_count_v1, &mut shards_v1).unwrap();

    let mut shards_v2 = empty_shards(shard_count_v2);
    shards_v2[shard_id_for_path(&file, shard_count_v2) as usize]
        .symbols
        .insert("A", sym("A", &file, 1, 1));
    save_sharded_indexes(&cache_dir, &snapshot, shard_count_v2, &mut shards_v2).unwrap();

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
    shards[shard_a]
        .symbols
        .insert("A", sym("A", "A.java", 1, 1));
    save_sharded_indexes(&cache_dir, &snapshot, shard_count, &mut shards).unwrap();

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

#[test]
fn lazy_sharded_fast_load_does_not_read_project_file_contents() {
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
    shards[shard_a]
        .symbols
        .insert("A", sym("A", "A.java", 1, 1));
    save_sharded_indexes(&cache_dir, &snapshot, shard_count, &mut shards).unwrap();

    // Replace the file with a directory. Reading contents would now fail, but metadata access
    // should still work.
    std::fs::remove_file(&a).unwrap();
    std::fs::create_dir_all(&a).unwrap();
    assert!(std::fs::read(&a).is_err());

    let loaded = load_sharded_index_view_lazy_fast(
        &cache_dir,
        &project_root,
        vec![PathBuf::from("A.java")],
        shard_count,
    )
    .unwrap();

    assert!(loaded.is_some());
}

#[test]
fn lazy_sharded_load_works_with_metadata_bin_only() {
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
    shards[shard_a]
        .symbols
        .insert("A", sym("A", "A.java", 1, 1));
    save_sharded_indexes(&cache_dir, &snapshot, shard_count, &mut shards).unwrap();

    // Simulate an interruption between writing `metadata.bin` and `metadata.json`.
    std::fs::remove_file(cache_dir.metadata_path()).unwrap();
    assert!(cache_dir.metadata_bin_path().exists());

    let loaded = load_sharded_index_view_lazy(&cache_dir, &snapshot, shard_count)
        .unwrap()
        .unwrap();
    assert!(loaded.invalidated_files.is_empty());
}

#[test]
fn sharded_load_works_with_metadata_bin_only() {
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
    shards[shard_a]
        .symbols
        .insert("A", sym("A", "A.java", 1, 1));
    save_sharded_indexes(&cache_dir, &snapshot, shard_count, &mut shards).unwrap();

    // Simulate an interruption between writing `metadata.bin` and `metadata.json`.
    std::fs::remove_file(cache_dir.metadata_path()).unwrap();
    assert!(cache_dir.metadata_bin_path().exists());

    let loaded = load_sharded_index_archives_fast(
        &cache_dir,
        &project_root,
        vec![PathBuf::from("A.java")],
        shard_count,
    )
    .unwrap();
    assert!(loaded.is_some());
}

#[test]
fn save_sharded_indexes_rewrites_only_affected_shards() {
    let shard_count = 64;

    let temp = tempfile::tempdir().unwrap();
    let project_root = temp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();

    // Pick two file names that land in different shards so we can observe that only the affected
    // shard is rewritten.
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
        std::fs::write(project_root.join(name), "class X {}").unwrap();
    }

    let snapshot_v1 =
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
        shards[shard]
            .symbols
            .insert(symbol, sym(symbol, name, 1, 1));
    }

    save_sharded_indexes(&cache_dir, &snapshot_v1, shard_count, &mut shards).unwrap();

    let affected_path = &paths[0];
    let unaffected_path = &paths[1];
    let affected_shard = shard_id_for_path(affected_path, shard_count);
    let unaffected_shard = shard_id_for_path(unaffected_path, shard_count);
    assert_ne!(affected_shard, unaffected_shard);

    let affected_symbols_idx = cache_dir
        .indexes_dir()
        .join("shards")
        .join(affected_shard.to_string())
        .join("symbols.idx");
    let unaffected_symbols_idx = cache_dir
        .indexes_dir()
        .join("shards")
        .join(unaffected_shard.to_string())
        .join("symbols.idx");

    let affected_mtime_v1 = std::fs::metadata(&affected_symbols_idx)
        .unwrap()
        .modified()
        .unwrap();
    let unaffected_mtime_v1 = std::fs::metadata(&unaffected_symbols_idx)
        .unwrap()
        .modified()
        .unwrap();

    // Ensure we cross any coarse filesystem timestamp boundaries.
    std::thread::sleep(Duration::from_millis(1100));

    std::fs::write(project_root.join(affected_path), "class X { void m() {} }").unwrap();
    let snapshot_v2 =
        ProjectSnapshot::new(&project_root, paths.iter().map(PathBuf::from).collect()).unwrap();

    // Simulate an incremental rebuild by updating only the affected shard payload.
    shards[affected_shard as usize]
        .symbols
        .insert("Updated", sym("Updated", affected_path, 2, 1));

    save_sharded_indexes(&cache_dir, &snapshot_v2, shard_count, &mut shards).unwrap();

    let affected_mtime_v2 = std::fs::metadata(&affected_symbols_idx)
        .unwrap()
        .modified()
        .unwrap();
    let unaffected_mtime_v2 = std::fs::metadata(&unaffected_symbols_idx)
        .unwrap()
        .modified()
        .unwrap();

    assert_ne!(affected_mtime_v2, affected_mtime_v1);
    assert_eq!(unaffected_mtime_v2, unaffected_mtime_v1);
}

#[test]
fn save_sharded_indexes_incremental_rewrites_only_touched_shards() {
    let shard_count = 64;

    let temp = tempfile::tempdir().unwrap();
    let project_root = temp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();

    // Pick two file names that land in different shards.
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
        std::fs::write(project_root.join(name), "class X {}").unwrap();
    }

    let snapshot_v1 =
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
        shards[shard]
            .symbols
            .insert(symbol, sym(symbol, name, 1, 1));
    }

    save_sharded_indexes(&cache_dir, &snapshot_v1, shard_count, &mut shards).unwrap();

    let affected_path = &paths[0];
    let unaffected_path = &paths[1];
    let affected_shard = shard_id_for_path(affected_path, shard_count);
    let unaffected_shard = shard_id_for_path(unaffected_path, shard_count);
    assert_ne!(affected_shard, unaffected_shard);

    let affected_symbols_idx = cache_dir
        .indexes_dir()
        .join("shards")
        .join(affected_shard.to_string())
        .join("symbols.idx");
    let unaffected_symbols_idx = cache_dir
        .indexes_dir()
        .join("shards")
        .join(unaffected_shard.to_string())
        .join("symbols.idx");

    let affected_mtime_v1 = std::fs::metadata(&affected_symbols_idx)
        .unwrap()
        .modified()
        .unwrap();
    let unaffected_mtime_v1 = std::fs::metadata(&unaffected_symbols_idx)
        .unwrap()
        .modified()
        .unwrap();

    std::thread::sleep(Duration::from_millis(1100));

    // Modify exactly one file so exactly one shard is affected.
    std::fs::write(project_root.join(affected_path), "class X { void m() {} }").unwrap();
    let snapshot_v2 =
        ProjectSnapshot::new(&project_root, paths.iter().map(PathBuf::from).collect()).unwrap();

    let base = load_sharded_index_archives(&cache_dir, &snapshot_v2, shard_count)
        .unwrap()
        .unwrap();
    assert_eq!(base.invalidated_files, vec![affected_path.clone()]);
    assert!(base.missing_shards.is_empty());

    let mut overlay = ShardedIndexOverlay::new(shard_count).unwrap();
    let mut delta = ProjectIndexes::default();
    delta
        .symbols
        .insert("Updated", sym("Updated", affected_path, 2, 1));
    overlay.apply_file_delta(affected_path, delta);

    save_sharded_indexes_incremental(&cache_dir, &snapshot_v2, shard_count, &base, &overlay)
        .unwrap();

    let affected_mtime_v2 = std::fs::metadata(&affected_symbols_idx)
        .unwrap()
        .modified()
        .unwrap();
    let unaffected_mtime_v2 = std::fs::metadata(&unaffected_symbols_idx)
        .unwrap()
        .modified()
        .unwrap();

    assert_ne!(affected_mtime_v2, affected_mtime_v1);
    assert_eq!(unaffected_mtime_v2, unaffected_mtime_v1);

    let loaded = load_sharded_index_view(&cache_dir, &snapshot_v2, shard_count)
        .unwrap()
        .unwrap();
    assert!(loaded.invalidated_files.is_empty());
    assert_eq!(
        loaded
            .view
            .symbol_locations("Updated")
            .map(|loc| loc.file)
            .collect::<Vec<_>>(),
        vec![affected_path.as_str()]
    );
}
