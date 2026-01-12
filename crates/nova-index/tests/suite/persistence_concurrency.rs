use nova_cache::{CacheConfig, CacheDir, ProjectSnapshot};
use nova_index::{
    load_indexes, save_indexes, IndexedSymbol, IndexSymbolKind, ProjectIndexes, ReferenceLocation,
    SymbolLocation,
};
use std::path::PathBuf;
use std::sync::{Arc, Barrier};

fn sym(name: String, file: &str, line: u32, column: u32) -> IndexedSymbol {
    IndexedSymbol {
        qualified_name: name,
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
fn concurrent_save_indexes_does_not_corrupt_cache_files() {
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

    let entries = 2_000;

    let mut indexes_a = ProjectIndexes::default();
    for i in 0..entries {
        indexes_a
            .symbols
            .insert(format!("S{i}"), sym(format!("S{i}"), "A.java", i as u32, 0));
        indexes_a.references.insert(
            format!("S{i}"),
            ReferenceLocation {
                file: "A.java".to_string(),
                line: i as u32,
                column: 1,
            },
        );
    }

    let mut indexes_b = ProjectIndexes::default();
    for i in 0..entries {
        indexes_b
            .symbols
            .insert(format!("T{i}"), sym(format!("T{i}"), "B.java", i as u32, 2));
        indexes_b.references.insert(
            format!("T{i}"),
            ReferenceLocation {
                file: "B.java".to_string(),
                line: i as u32,
                column: 3,
            },
        );
    }

    let barrier = Arc::new(Barrier::new(3));

    let cache_dir_a = cache_dir.clone();
    let snapshot_a = snapshot.clone();
    let barrier_a = barrier.clone();
    let handle_a = std::thread::spawn(move || {
        let mut indexes_a = indexes_a;
        barrier_a.wait();
        save_indexes(&cache_dir_a, &snapshot_a, &mut indexes_a).unwrap();
    });

    let cache_dir_b = cache_dir.clone();
    let snapshot_b = snapshot.clone();
    let barrier_b = barrier.clone();
    let handle_b = std::thread::spawn(move || {
        let mut indexes_b = indexes_b;
        barrier_b.wait();
        save_indexes(&cache_dir_b, &snapshot_b, &mut indexes_b).unwrap();
    });

    // Release both writers at once.
    barrier.wait();

    handle_a.join().unwrap();
    handle_b.join().unwrap();

    let loaded = load_indexes(&cache_dir, &snapshot).unwrap().unwrap();
    assert!(loaded.invalidated_files.is_empty());
    assert_eq!(loaded.indexes.symbols.symbols.len(), entries);

    // The final value is nondeterministic (last writer wins), but it must be
    // readable and internally consistent.
    let symbols = &loaded.indexes.symbols.symbols;
    assert!(
        symbols.contains_key("S0") || symbols.contains_key("T0"),
        "expected symbols from one of the writers"
    );
}
