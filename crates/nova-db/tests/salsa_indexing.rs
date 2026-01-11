use std::sync::Arc;

use nova_cache::CacheConfig;
use nova_db::{FileId, NovaIndexing, PersistenceConfig, PersistenceMode, ProjectId, SalsaDatabase};
use nova_index::ProjectIndexes;

fn executions(db: &SalsaDatabase, query_name: &str) -> u64 {
    db.query_stats()
        .by_query
        .get(query_name)
        .map(|s| s.executions)
        .unwrap_or(0)
}

fn has_symbol(shards: &[ProjectIndexes], symbol: &str) -> bool {
    shards
        .iter()
        .any(|shard| shard.symbols.symbols.contains_key(symbol))
}

fn symbol_locations(shards: &[ProjectIndexes], symbol: &str) -> Vec<nova_index::SymbolLocation> {
    let mut out = Vec::new();
    for shard in shards {
        if let Some(locs) = shard.symbols.symbols.get(symbol) {
            out.extend(locs.iter().cloned());
        }
    }
    out
}

#[test]
fn project_indexes_warm_start_and_invalidation() {
    let tmp = tempfile::tempdir().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();

    let cache_root = tmp.path().join("cache-root");
    std::fs::create_dir_all(&cache_root).unwrap();
    let persistence = PersistenceConfig {
        mode: PersistenceMode::ReadWrite,
        cache: CacheConfig {
            cache_root_override: Some(cache_root),
        },
    };

    let project = ProjectId::from_raw(0);
    let a = FileId::from_raw(1);
    let b = FileId::from_raw(2);

    // First run: build indexes from scratch and persist.
    let db1 = SalsaDatabase::new_with_persistence(&project_root, persistence.clone());
    db1.set_project_files(project, Arc::new(vec![a, b]));
    db1.set_file_rel_path(a, Arc::new("A.java".to_string()));
    db1.set_file_rel_path(b, Arc::new("B.java".to_string()));
    db1.set_file_text(a, "class A {}".to_string());
    db1.set_file_text(b, "class B {}".to_string());

    let shards_v1 = db1.with_snapshot(|snap| (*snap.project_indexes(project)).clone());
    assert!(has_symbol(&shards_v1, "A"));
    assert!(has_symbol(&shards_v1, "B"));

    db1.persist_project_indexes(project).unwrap();

    // Second run: warm-start should load indexes without re-indexing unchanged files.
    let db2 = SalsaDatabase::new_with_persistence(&project_root, persistence);
    db2.set_project_files(project, Arc::new(vec![a, b]));
    db2.set_file_rel_path(a, Arc::new("A.java".to_string()));
    db2.set_file_rel_path(b, Arc::new("B.java".to_string()));
    db2.set_file_text(a, "class A {}".to_string());
    db2.set_file_text(b, "class B {}".to_string());

    db2.clear_query_stats();
    let shards_v2 = db2.with_snapshot(|snap| (*snap.project_indexes(project)).clone());

    assert_eq!(shards_v2, shards_v1);
    assert_eq!(executions(&db2, "file_index_delta"), 0);
    assert_eq!(executions(&db2, "parse_java"), 0);

    // Change one file so its fingerprint changes; only that file should be re-indexed.
    db2.clear_query_stats();
    db2.set_file_text(b, "class B { class C {} }".to_string());
    let shards_v3 = db2.with_snapshot(|snap| (*snap.project_indexes(project)).clone());

    assert_eq!(executions(&db2, "file_index_delta"), 1);
    assert_eq!(executions(&db2, "parse_java"), 1);
    assert!(has_symbol(&shards_v3, "C"));
    assert!(symbol_locations(&shards_v3, "C")
        .iter()
        .all(|loc| loc.file == "B.java"));
}

#[test]
fn project_indexes_early_cutoff_on_whitespace_edit() {
    let project = ProjectId::from_raw(0);
    let file = FileId::from_raw(1);

    let db = SalsaDatabase::new();
    db.set_project_files(project, Arc::new(vec![file]));
    db.set_file_rel_path(file, Arc::new("Foo.java".to_string()));
    db.set_file_text(file, "class Foo {}".to_string());

    let count1 = db.with_snapshot(|snap| snap.project_symbol_count(project));
    assert_eq!(count1, 1);
    assert_eq!(executions(&db, "project_symbol_count"), 1);
    assert_eq!(executions(&db, "file_index_delta"), 1);

    db.set_file_content(file, Arc::new("  class Foo {}".to_string()));
    let count2 = db.with_snapshot(|snap| snap.project_symbol_count(project));
    assert_eq!(count2, count1);

    assert_eq!(
        executions(&db, "project_symbol_count"),
        1,
        "dependent query should be reused due to early-cutoff"
    );
    assert_eq!(
        executions(&db, "file_index_delta"),
        1,
        "file delta should not recompute when symbol summary is unchanged"
    );
}
