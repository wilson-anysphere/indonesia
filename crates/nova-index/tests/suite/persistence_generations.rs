use nova_cache::{CacheConfig, CacheDir, ProjectSnapshot};
use nova_index::{
    load_index_archives, save_indexes, IndexSymbolKind, IndexedSymbol, ProjectIndexes,
    SymbolLocation, INDEX_SCHEMA_VERSION,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::time::Duration;

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
fn mixed_generation_is_cache_miss() {
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
    indexes.symbols.insert("A", sym("A", "A.java", 1, 1));
    save_indexes(&cache_dir, &snapshot, &mut indexes).unwrap();
    let generation_v1 = indexes.symbols.generation;

    // Simulate a crash mid-update by rewriting only one index file with a new generation.
    let mut symbols_v2 = indexes.symbols.clone();
    symbols_v2.generation = generation_v1 + 1;
    nova_storage::write_archive_atomic(
        &cache_dir.indexes_dir().join("symbols.idx"),
        nova_storage::ArtifactKind::SymbolIndex,
        INDEX_SCHEMA_VERSION,
        &symbols_v2,
        nova_storage::Compression::None,
    )
    .unwrap();

    let loaded = load_index_archives(&cache_dir, &snapshot).unwrap();
    assert!(loaded.is_none());
}

#[test]
fn concurrent_writers_never_produce_mixed_generation_loads() {
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

    // Seed the cache so the loader has something to observe while writers race.
    let mut seed = ProjectIndexes::default();
    seed.symbols.insert("Seed", sym("Seed", "A.java", 1, 1));
    save_indexes(&cache_dir, &snapshot, &mut seed).unwrap();

    let cache_dir = Arc::new(cache_dir);
    let snapshot = Arc::new(snapshot);

    let generations: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
    let successful_loads = Arc::new(AtomicUsize::new(0));

    let thread_count = 4;
    let iterations = 10;
    let barrier = Arc::new(Barrier::new(thread_count));

    let mut handles = Vec::new();
    for thread_id in 0..thread_count {
        let cache_dir = cache_dir.clone();
        let snapshot = snapshot.clone();
        let generations = generations.clone();
        let successful_loads = successful_loads.clone();
        let barrier = barrier.clone();
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            for iter in 0..iterations {
                let mut indexes = ProjectIndexes::default();
                // Add enough data to make each write non-trivial, increasing the chance
                // that other threads observe a mid-save directory state.
                for entry in 0..200u32 {
                    let name = format!("T{thread_id}-I{iter}-S{entry}");
                    indexes
                        .symbols
                        .insert(name.clone(), sym(&name, "A.java", entry + 1, 1));
                }

                save_indexes(&cache_dir, &snapshot, &mut indexes).unwrap();
                generations
                    .lock()
                    .unwrap_or_else(|err| err.into_inner())
                    .push(indexes.symbols.generation);

                // Give other writers a chance to start saving while we load.
                std::thread::sleep(Duration::from_millis(1));

                if let Some(archives) = load_index_archives(&cache_dir, &snapshot).unwrap() {
                    let generation = archives.symbols.generation;
                    assert_eq!(archives.references.generation, generation);
                    assert_eq!(archives.inheritance.generation, generation);
                    assert_eq!(archives.annotations.generation, generation);
                    successful_loads.fetch_add(1, Ordering::Relaxed);
                }
            }
        }));
    }

    for handle in handles {
        handle.join().unwrap();
    }

    let successful_loads = successful_loads.load(Ordering::Relaxed);
    assert!(
        successful_loads > 0,
        "expected at least one successful load"
    );

    let generations = generations.lock().unwrap_or_else(|err| err.into_inner());
    let mut unique = generations.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), generations.len());
}
