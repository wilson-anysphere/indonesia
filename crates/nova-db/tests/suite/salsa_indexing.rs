use std::sync::Arc;

use nova_cache::{CacheConfig, CacheDir};
use nova_db::{
    FileId, NovaHir, NovaIndexing, NovaSyntax, PersistenceConfig, PersistenceMode, ProjectId,
    SalsaDatabase,
};
use nova_memory::MemoryPressure;

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
    std::fs::write(project_root.join("A.java"), "class A {}").unwrap();
    std::fs::write(project_root.join("B.java"), "class B {}").unwrap();

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
    std::fs::write(project_root.join("B.java"), "class B { class C {} }").unwrap();
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
        .all(|loc| loc.location.file == "B.java"));
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
fn project_indexes_warm_start_reindexes_dirty_file_even_if_disk_metadata_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();

    // Persisted state corresponds to this on-disk snapshot.
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

    // Second run: keep the on-disk files unchanged, but provide an in-memory edit for B.java.
    // The dirty flag must force warm-start invalidation even though disk metadata matches.
    let db2 = SalsaDatabase::new_with_persistence(&project_root, persistence);
    db2.set_project_files(project, Arc::new(vec![a, b]));
    db2.set_file_rel_path(a, Arc::new("A.java".to_string()));
    db2.set_file_rel_path(b, Arc::new("B.java".to_string()));
    db2.set_file_text(a, "class A {}".to_string());
    db2.set_file_text(b, "class B {}".to_string());
    db2.set_file_content(b, Arc::new("class B { class C {} }".to_string()));
    db2.set_file_is_dirty(b, true);

    db2.clear_query_stats();
    let indexes_v2 = db2.with_snapshot(|snap| (*snap.project_indexes(project)).clone());

    assert!(indexes_v2.symbols.symbols.contains_key("C"));
    assert!(indexes_v2
        .symbols
        .symbols
        .get("C")
        .unwrap()
        .iter()
        .all(|loc| loc.location.file == "B.java"));

    assert_eq!(
        executions(&db2, "file_index_delta"),
        1,
        "expected only the dirty file to be re-indexed"
    );
    assert_eq!(
        executions(&db2, "parse_java"),
        1,
        "expected only the dirty file to be reparsed"
    );
    assert_eq!(
        executions(&db2, "file_fingerprint"),
        1,
        "expected warm-start to hash only the dirty file"
    );

    assert_ne!(indexes_v2, indexes_v1);
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
fn persist_project_indexes_is_noop_when_dirty_files_exist() {
    let tmp = tempfile::tempdir().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();

    std::fs::write(project_root.join("A.java"), "class A {}").unwrap();
    std::fs::write(project_root.join("B.java"), "class B {}").unwrap();

    let cache_root = tmp.path().join("cache-root");
    std::fs::create_dir_all(&cache_root).unwrap();
    let cache = CacheConfig {
        cache_root_override: Some(cache_root),
    };
    let persistence = PersistenceConfig {
        mode: PersistenceMode::ReadWrite,
        cache: cache.clone(),
    };

    let project = ProjectId::from_raw(0);
    let a = FileId::from_raw(1);
    let b = FileId::from_raw(2);

    let db = SalsaDatabase::new_with_persistence(&project_root, persistence);
    db.set_project_files(project, Arc::new(vec![a, b]));
    db.set_file_rel_path(a, Arc::new("A.java".to_string()));
    db.set_file_rel_path(b, Arc::new("B.java".to_string()));
    db.set_file_text(a, "class A {}".to_string());
    db.set_file_text(b, "class B {}".to_string());
    db.set_file_is_dirty(b, true);

    db.persist_project_indexes(project).unwrap();

    let cache_dir = CacheDir::new(&project_root, cache).unwrap();
    let manifest = cache_dir.indexes_dir().join("shards").join("manifest.txt");
    assert!(
        !manifest.exists(),
        "persist_project_indexes should not write sharded index artifacts when dirty files exist"
    );
    assert!(
        !cache_dir.metadata_path().exists(),
        "persist_project_indexes should not write project metadata when dirty files exist"
    );
    assert!(
        !cache_dir.metadata_bin_path().exists(),
        "persist_project_indexes should not write project metadata archive when dirty files exist"
    );
}

#[test]
fn persist_project_indexes_reuses_cached_fingerprints_for_unchanged_files() {
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

    // First run: persist indexes + fingerprints for all files.
    {
        let db = SalsaDatabase::new_with_persistence(&project_root, persistence.clone());
        db.set_project_files(project, Arc::new(vec![a, b]));
        db.set_file_rel_path(a, Arc::new("A.java".to_string()));
        db.set_file_rel_path(b, Arc::new("B.java".to_string()));
        db.set_file_text(a, "class A {}".to_string());
        db.set_file_text(b, "class B {}".to_string());

        db.persist_project_indexes(project).unwrap();
    }

    // Second run: modify one file; persistence should only hash that file by reusing fingerprints
    // from the on-disk metadata for unchanged files.
    std::fs::write(project_root.join("B.java"), "class B { class C {} }").unwrap();
    let db2 = SalsaDatabase::new_with_persistence(&project_root, persistence);
    db2.set_project_files(project, Arc::new(vec![a, b]));
    db2.set_file_rel_path(a, Arc::new("A.java".to_string()));
    db2.set_file_rel_path(b, Arc::new("B.java".to_string()));
    db2.set_file_text(a, "class A {}".to_string());
    db2.set_file_text(b, "class B { class C {} }".to_string());

    db2.clear_query_stats();
    db2.persist_project_indexes(project).unwrap();

    assert_eq!(
        executions(&db2, "file_fingerprint"),
        1,
        "expected persistence to hash only the invalidated file"
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

#[test]
fn file_index_delta_is_accounted_in_salsa_memo_bytes() {
    let file = FileId::from_raw(1);

    let db = SalsaDatabase::new();
    db.set_file_text(
        file,
        "class Foo { int x; void bar() {} class Inner {} } class Baz { String s; }".to_string(),
    );
    db.set_file_rel_path(file, Arc::new("Foo.java".to_string()));

    // Precompute dependencies so `bytes_before` is stable and does not include
    // the index delta.
    db.with_snapshot(|snap| {
        let _ = snap.parse_java(file);
        let _ = snap.hir_item_tree(file);
    });
    let bytes_before = db.salsa_memo_bytes();

    let delta = db.with_snapshot(|snap| snap.file_index_delta(file).clone());
    let delta_bytes = delta.estimated_bytes();
    assert!(delta_bytes > 0, "expected non-zero index delta footprint");

    let bytes_after = db.salsa_memo_bytes();
    assert!(
        bytes_after >= bytes_before.saturating_add(delta_bytes),
        "expected memo bytes to include file_index_delta estimate (before={bytes_before}, after={bytes_after}, delta={delta_bytes})"
    );

    db.evict_salsa_memos(MemoryPressure::Critical);
    assert_eq!(
        db.salsa_memo_bytes(),
        0,
        "expected memo footprint to be cleared after eviction"
    );

    // Queries should still recompute successfully after eviction.
    let delta2 = db.with_snapshot(|snap| snap.file_index_delta(file).clone());
    assert!(
        delta2.estimated_bytes() > 0,
        "expected index delta to recompute after eviction"
    );
}

#[test]
fn project_index_shards_are_accounted_in_salsa_memo_bytes() {
    let project = ProjectId::from_raw(0);
    let a = FileId::from_raw(1);
    let b = FileId::from_raw(2);

    let db = SalsaDatabase::new();
    db.set_project_files(project, Arc::new(vec![a, b]));
    db.set_file_rel_path(a, Arc::new("A.java".to_string()));
    db.set_file_rel_path(b, Arc::new("B.java".to_string()));
    db.set_file_text(a, "class A {}".to_string());
    db.set_file_text(b, "class B { class C {} }".to_string());

    // Precompute dependencies so the baseline excludes the project-level shard memo.
    db.with_snapshot(|snap| {
        let _ = snap.parse_java(a);
        let _ = snap.parse_java(b);
        let _ = snap.hir_item_tree(a);
        let _ = snap.hir_item_tree(b);
        let _ = snap.file_index_delta(a);
        let _ = snap.file_index_delta(b);
    });
    let bytes_before = db.salsa_memo_bytes();

    let shards = db.with_snapshot(|snap| snap.project_index_shards(project));
    let shards_bytes = shards.iter().fold(0u64, |total, shard| {
        total.saturating_add(shard.estimated_bytes())
    });
    assert!(
        shards_bytes > 0,
        "expected non-zero project_index_shards footprint"
    );

    let bytes_after = db.salsa_memo_bytes();
    assert!(
        bytes_after >= bytes_before.saturating_add(shards_bytes),
        "expected memo bytes to include project_index_shards estimate (before={bytes_before}, after={bytes_after}, shards={shards_bytes})"
    );

    db.evict_salsa_memos(MemoryPressure::Critical);
    assert_eq!(
        db.salsa_memo_bytes(),
        0,
        "expected memo footprint to be cleared after eviction"
    );
}

#[test]
fn project_indexes_are_accounted_in_salsa_memo_bytes() {
    let project = ProjectId::from_raw(0);
    let a = FileId::from_raw(1);
    let b = FileId::from_raw(2);

    let db = SalsaDatabase::new();
    db.set_project_files(project, Arc::new(vec![a, b]));
    db.set_file_rel_path(a, Arc::new("A.java".to_string()));
    db.set_file_rel_path(b, Arc::new("B.java".to_string()));
    db.set_file_text(a, "class A {}".to_string());
    db.set_file_text(b, "class B { class C {} }".to_string());

    // Baseline: compute and account for shard indexes first.
    let _ = db.with_snapshot(|snap| snap.project_index_shards(project));
    let bytes_before = db.salsa_memo_bytes();

    let indexes = db.with_snapshot(|snap| snap.project_indexes(project));
    let indexes_bytes = indexes.estimated_bytes();
    assert!(
        indexes_bytes > 0,
        "expected non-zero project_indexes footprint"
    );

    let bytes_after = db.salsa_memo_bytes();
    assert!(
        bytes_after >= bytes_before.saturating_add(indexes_bytes),
        "expected memo bytes to include project_indexes estimate (before={bytes_before}, after={bytes_after}, indexes={indexes_bytes})"
    );
}
