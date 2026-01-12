use std::sync::Arc;

use nova_cache::CacheConfig;
use nova_db::{FileId, NovaIndexing, PersistenceConfig, PersistenceMode, ProjectId, SalsaDatabase};

fn executions(db: &SalsaDatabase, query_name: &str) -> u64 {
    db.query_stats()
        .by_query
        .get(query_name)
        .map(|s| s.executions)
        .unwrap_or(0)
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

    let indexes_v1 = db1.with_snapshot(|snap| (*snap.project_indexes(project)).clone());
    assert!(indexes_v1.symbols.symbols.contains_key("A"));
    assert!(indexes_v1.symbols.symbols.contains_key("B"));

    db1.persist_project_indexes(project).unwrap();

    // Second run: warm-start should load indexes without re-indexing unchanged files.
    let db2 = SalsaDatabase::new_with_persistence(&project_root, persistence);
    db2.set_project_files(project, Arc::new(vec![a, b]));
    db2.set_file_rel_path(a, Arc::new("A.java".to_string()));
    db2.set_file_rel_path(b, Arc::new("B.java".to_string()));
    db2.set_file_text(a, "class A {}".to_string());
    db2.set_file_text(b, "class B {}".to_string());

    db2.clear_query_stats();
    let indexes_v2 = db2.with_snapshot(|snap| (*snap.project_indexes(project)).clone());

    assert_eq!(indexes_v2, indexes_v1);
    assert_eq!(executions(&db2, "file_index_delta"), 0);
    assert_eq!(executions(&db2, "parse_java"), 0);

    // Change one file so its fingerprint changes; only that file should be re-indexed.
    db2.clear_query_stats();
    db2.set_file_text(b, "class B { class C {} }".to_string());
    let indexes_v3 = db2.with_snapshot(|snap| (*snap.project_indexes(project)).clone());

    assert_eq!(executions(&db2, "file_index_delta"), 1);
    assert_eq!(executions(&db2, "parse_java"), 1);
    assert!(indexes_v3.symbols.symbols.contains_key("C"));
    assert!(indexes_v3
        .symbols
        .symbols
        .get("C")
        .unwrap()
        .iter()
        .all(|loc| loc.file == "B.java"));
}

#[test]
fn project_indexes_warm_start_avoids_file_fingerprint_for_unchanged_disk_files() {
    let tmp = tempfile::tempdir().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();

    std::fs::write(project_root.join("A.java"), "class A {}").unwrap();
    std::fs::write(project_root.join("B.java"), "class B {}").unwrap();

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

    let indexes_v1 = db1.with_snapshot(|snap| (*snap.project_indexes(project)).clone());
    assert!(indexes_v1.symbols.symbols.contains_key("A"));
    assert!(indexes_v1.symbols.symbols.contains_key("B"));
    db1.persist_project_indexes(project).unwrap();

    // Second run: warm-start should be able to validate the cache without hashing full contents
    // for unchanged on-disk files.
    let db2 = SalsaDatabase::new_with_persistence(&project_root, persistence);
    db2.set_project_files(project, Arc::new(vec![a, b]));
    db2.set_file_rel_path(a, Arc::new("A.java".to_string()));
    db2.set_file_rel_path(b, Arc::new("B.java".to_string()));
    db2.set_file_text(a, "class A {}".to_string());
    db2.set_file_text(b, "class B {}".to_string());

    db2.clear_query_stats();
    let indexes_v2 = db2.with_snapshot(|snap| (*snap.project_indexes(project)).clone());

    assert_eq!(indexes_v2, indexes_v1);
    assert_eq!(executions(&db2, "file_index_delta"), 0);
    assert_eq!(executions(&db2, "parse_java"), 0);
    assert_eq!(
        executions(&db2, "file_fingerprint"),
        0,
        "warm-start should validate unchanged on-disk files via metadata fingerprints"
    );
}

#[test]
fn project_index_shards_has_fixed_len_and_merges_to_project_indexes() {
    let project = ProjectId::from_raw(0);
    let a = FileId::from_raw(1);
    let b = FileId::from_raw(2);

    let db = SalsaDatabase::new();
    db.set_project_files(project, Arc::new(vec![a, b]));
    db.set_file_rel_path(a, Arc::new("A.java".to_string()));
    db.set_file_rel_path(b, Arc::new("B.java".to_string()));
    db.set_file_text(a, "class A {}".to_string());
    db.set_file_text(b, "class B {}".to_string());

    let shards = db.with_snapshot(|snap| (*snap.project_index_shards(project)).clone());
    assert_eq!(shards.len(), nova_index::DEFAULT_SHARD_COUNT as usize);

    let merged = db.with_snapshot(|snap| (*snap.project_indexes(project)).clone());
    let mut manual = nova_index::ProjectIndexes::default();
    for shard in &shards {
        manual.merge_from(shard.clone());
    }
    manual.set_generation(0);
    assert_eq!(manual, merged);

    let shard_id =
        nova_index::shard_id_for_path("A.java", nova_index::DEFAULT_SHARD_COUNT) as usize;
    assert!(
        shards[shard_id].symbols.symbols.contains_key("A"),
        "expected A.java symbols to be placed in its deterministic shard"
    );
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
