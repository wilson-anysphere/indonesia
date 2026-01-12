use nova_cache::{CacheConfig, CacheDir, ProjectSnapshot};
use nova_index::{
    append_index_segment, compact_index_segments, load_index_archives, load_index_view,
    save_indexes, IndexedSymbol, IndexSymbolKind, ProjectIndexes, SymbolLocation,
};
use std::path::PathBuf;

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
fn segments_overlay_and_compaction() {
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

    let mut base = ProjectIndexes::default();
    base.symbols.insert("A", sym("A", "A.java", 1, 1));
    base.symbols.insert("B", sym("B", "B.java", 1, 1));

    save_indexes(&cache_dir, &snapshot_v1, &mut base).unwrap();

    // Update A.java and persist a delta segment that supersedes its base entries.
    std::fs::write(&a, "class A2 {}").unwrap();
    let snapshot_v2 = ProjectSnapshot::new(
        &project_root,
        vec![PathBuf::from("A.java"), PathBuf::from("B.java")],
    )
    .unwrap();

    let mut delta = ProjectIndexes::default();
    delta.symbols.insert("A2", sym("A2", "A.java", 1, 1));

    append_index_segment(
        &cache_dir,
        &snapshot_v2,
        &["A.java".to_string()],
        &mut delta,
    )
    .unwrap();

    let view_with_segment = load_index_view(&cache_dir, &snapshot_v2).unwrap().unwrap();
    assert!(view_with_segment.invalidated_files.is_empty());
    assert!(view_with_segment.symbol_locations("A").next().is_none());
    assert_eq!(view_with_segment.symbol_locations("A2").count(), 1);
    assert_eq!(view_with_segment.symbol_locations("B").count(), 1);
    assert_eq!(
        view_with_segment.symbol_names().collect::<Vec<_>>(),
        vec!["A2", "B"]
    );

    let store = load_index_archives(&cache_dir, &snapshot_v2)
        .unwrap()
        .expect("expected base+segment store");
    assert_eq!(store.segments.len(), 1);
    assert_eq!(store.file_to_segment.get("A.java").copied(), Some(0));
    assert!(store.symbol_locations("A").is_empty());
    assert_eq!(
        store.symbol_locations("A2"),
        vec![SymbolLocation {
            file: "A.java".to_string(),
            line: 1,
            column: 1,
        }]
    );
    assert_eq!(
        store.symbol_locations("B"),
        vec![SymbolLocation {
            file: "B.java".to_string(),
            line: 1,
            column: 1,
        }]
    );

    compact_index_segments(&cache_dir).unwrap();

    let segments_dir = cache_dir.indexes_dir().join("segments");
    assert!(
        !segments_dir.exists(),
        "expected compaction to clear {}",
        segments_dir.display()
    );

    let compacted = load_index_archives(&cache_dir, &snapshot_v2)
        .unwrap()
        .expect("expected compacted store");
    assert!(compacted.segments.is_empty());
    assert!(compacted.file_to_segment.is_empty());
    assert!(compacted.symbol_locations("A").is_empty());
    assert_eq!(
        compacted.symbol_locations("A2"),
        vec![SymbolLocation {
            file: "A.java".to_string(),
            line: 1,
            column: 1,
        }]
    );
    assert_eq!(
        compacted.symbol_locations("B"),
        vec![SymbolLocation {
            file: "B.java".to_string(),
            line: 1,
            column: 1,
        }]
    );

    let view_after_compaction = load_index_view(&cache_dir, &snapshot_v2).unwrap().unwrap();
    assert!(view_after_compaction.symbol_locations("A").next().is_none());
    assert_eq!(view_after_compaction.symbol_locations("A2").count(), 1);
}
