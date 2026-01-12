use std::path::PathBuf;

use criterion::{black_box, criterion_group, criterion_main, Criterion};

use nova_cache::{CacheConfig, CacheDir, ProjectSnapshot};
use nova_index::{load_index_archives, load_indexes, save_indexes, ProjectIndexes, SymbolLocation};

use nova_index::{IndexSymbolKind, IndexedSymbol};

fn build_symbol_index_entries(indexes: &mut ProjectIndexes, file: &str, count: usize) {
    for i in 0..count {
        let symbol = format!("Symbol{i:08}");

        let location = SymbolLocation {
            file: file.to_string(),
            line: (i as u32) + 1,
            column: 1,
        };

        indexes.symbols.insert(
            symbol.clone(),
            IndexedSymbol {
                qualified_name: symbol,
                kind: IndexSymbolKind::Class,
                container_name: None,
                location,
                ast_id: i as u32,
            },
        );
    }
}

fn count_prefix_archived(
    archive: &nova_storage::PersistedArchive<nova_index::SymbolIndex>,
    prefix: &str,
) -> usize {
    let mut count = 0usize;
    for (name, _locations) in archive.archived().symbols.iter() {
        if name.as_str().starts_with(prefix) {
            count += 1;
        }
    }
    count
}

fn count_prefix_owned(index: &nova_index::SymbolIndex, prefix: &str) -> usize {
    index
        .symbols
        .keys()
        .filter(|name| name.starts_with(prefix))
        .count()
}

fn bench_mmap_storage(c: &mut Criterion) {
    let tmp = tempfile::TempDir::new().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();

    let file_a = project_root.join("A.java");
    std::fs::write(&file_a, "class A {}").unwrap();

    let snapshot = ProjectSnapshot::new(&project_root, vec![PathBuf::from("A.java")]).unwrap();

    let cache_dir = CacheDir::new(
        &project_root,
        CacheConfig {
            cache_root_override: Some(tmp.path().join("cache-root")),
        },
    )
    .unwrap();

    let mut indexes = ProjectIndexes::default();
    build_symbol_index_entries(&mut indexes, "A.java", 50_000);
    save_indexes(&cache_dir, &snapshot, &mut indexes).unwrap();

    c.bench_function("indexes_load_archives", |b| {
        b.iter(|| {
            black_box(load_index_archives(&cache_dir, &snapshot).unwrap().unwrap());
        })
    });

    c.bench_function("indexes_load_owned", |b| {
        b.iter(|| {
            black_box(load_indexes(&cache_dir, &snapshot).unwrap().unwrap());
        })
    });

    let archives = load_index_archives(&cache_dir, &snapshot).unwrap().unwrap();
    let owned = load_indexes(&cache_dir, &snapshot).unwrap().unwrap();

    c.bench_function("symbol_search_prefix_archived", |b| {
        b.iter(|| black_box(count_prefix_archived(&archives.symbols, "Symbol0001")))
    });

    c.bench_function("symbol_search_prefix_owned", |b| {
        b.iter(|| black_box(count_prefix_owned(&owned.indexes.symbols, "Symbol0001")))
    });
}

criterion_group!(benches, bench_mmap_storage);
criterion_main!(benches);
