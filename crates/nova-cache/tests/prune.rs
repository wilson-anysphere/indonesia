use bincode::Options;
use nova_cache::{
    prune_cache, AstArtifactCache, CacheConfig, CacheDir, CacheMetadata, DerivedArtifactCache,
    FileAstArtifacts, Fingerprint, ProjectSnapshot, PrunePolicy,
};
use nova_hir::token_item_tree::token_item_tree;
use nova_syntax::parse;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn new_cache_dir() -> (tempfile::TempDir, CacheDir) {
    let tmp = tempfile::tempdir().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(project_root.join("src")).unwrap();
    std::fs::write(project_root.join("src/Main.java"), b"class Main {}").unwrap();

    let cache_root = tmp.path().join("cache-root");
    let cache_dir = CacheDir::new(
        &project_root,
        CacheConfig {
            cache_root_override: Some(cache_root),
        },
    )
    .unwrap();

    let snapshot =
        ProjectSnapshot::new(&project_root, vec![PathBuf::from("src/Main.java")]).unwrap();
    let metadata = CacheMetadata::new(&snapshot);
    metadata.save(cache_dir.metadata_path()).unwrap();

    (tmp, cache_dir)
}

fn store_ast(cache_dir: &CacheDir, file_path: &str, text: &str) -> String {
    let cache = AstArtifactCache::new(cache_dir.ast_dir());
    let parsed = parse(text);
    let it = token_item_tree(&parsed, text);
    let artifacts = FileAstArtifacts {
        parse: parsed,
        item_tree: it,
        symbol_summary: None,
    };
    let fp = Fingerprint::from_bytes(text.as_bytes());
    cache.store(file_path, &fp, &artifacts).unwrap();

    let key = Fingerprint::from_bytes(file_path.as_bytes());
    format!("{}.ast", key.as_str())
}

fn rewrite_ast_saved_at(cache_dir: &CacheDir, file_path: &str, saved_at_millis: u64) {
    let metadata_path = cache_dir.ast_dir().join("metadata.bin");
    let bytes = std::fs::read(&metadata_path).unwrap();
    let mut metadata: TestAstCacheMetadata = ast_bincode_options().deserialize(&bytes).unwrap();
    metadata.files.get_mut(file_path).unwrap().saved_at_millis = saved_at_millis;
    let bytes = ast_bincode_options().serialize(&metadata).unwrap();
    std::fs::write(&metadata_path, bytes).unwrap();
}

fn rewrite_query_saved_at<T: Serialize + for<'de> Deserialize<'de>>(
    entry_path: &Path,
    saved_at_millis: u64,
) {
    let bytes = std::fs::read(entry_path).unwrap();
    let mut persisted: PersistedDerivedValueOwned<T> = bincode::deserialize(&bytes).unwrap();
    persisted.saved_at_millis = saved_at_millis;
    let bytes = bincode::serialize(&persisted).unwrap();
    std::fs::write(entry_path, bytes).unwrap();
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedDerivedValueOwned<T> {
    schema_version: u32,
    nova_version: String,
    saved_at_millis: u64,
    query_name: String,
    key_fingerprint: Fingerprint,
    value: T,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TestAstCacheMetadata {
    schema_version: u32,
    syntax_schema_version: u32,
    hir_schema_version: u32,
    nova_version: String,
    files: BTreeMap<String, TestAstCacheFileEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TestAstCacheFileEntry {
    fingerprint: Fingerprint,
    artifact_file: String,
    saved_at_millis: u64,
}

fn ast_bincode_options() -> impl bincode::Options {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .with_no_limit()
}

#[derive(Debug, Serialize, Deserialize)]
struct Args {
    file: String,
}

#[test]
fn prunes_unreferenced_ast_artifacts() {
    let (_tmp, cache_dir) = new_cache_dir();
    let artifact_name = store_ast(&cache_dir, "src/Foo.java", "class Foo {}");

    let dead = cache_dir.ast_dir().join("dead.ast");
    std::fs::write(&dead, b"orphan").unwrap();
    assert!(dead.is_file());

    let report = prune_cache(&cache_dir, PrunePolicy::default()).unwrap();

    assert!(cache_dir.ast_dir().join(artifact_name).is_file());
    assert!(!dead.exists());
    assert!(cache_dir.metadata_path().is_file());
    assert!(cache_dir.ast_dir().join("metadata.bin").is_file());
    assert_eq!(report.deleted_files, 1);
}

#[test]
fn prunes_referenced_ast_artifacts_older_than_max_age() {
    let (_tmp, cache_dir) = new_cache_dir();
    let artifact_name = store_ast(&cache_dir, "src/Foo.java", "class Foo {}");
    rewrite_ast_saved_at(&cache_dir, "src/Foo.java", 0);

    let report = prune_cache(
        &cache_dir,
        PrunePolicy {
            max_age_days: Some(1),
            ..PrunePolicy::default()
        },
    )
    .unwrap();

    assert!(!cache_dir.ast_dir().join(&artifact_name).exists());

    let metadata_path = cache_dir.ast_dir().join("metadata.bin");
    let bytes = std::fs::read(&metadata_path).unwrap();
    let metadata: TestAstCacheMetadata = ast_bincode_options().deserialize(&bytes).unwrap();
    assert!(metadata.files.is_empty());

    assert!(cache_dir.metadata_path().is_file());
    assert!(report.deleted_files >= 1);
}

#[test]
fn prunes_query_entries_older_than_max_age() {
    let (_tmp, cache_dir) = new_cache_dir();
    let cache = DerivedArtifactCache::new(cache_dir.queries_dir());

    let mut inputs = BTreeMap::new();
    inputs.insert("Main.java".to_string(), Fingerprint::from_bytes("v1"));

    let args1 = Args {
        file: "Main.java".to_string(),
    };
    cache.store("type_of", &args1, &inputs, &42u32).unwrap();

    let query_dir = cache_dir.queries_dir().join("type_of");
    let old_path = std::fs::read_dir(&query_dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    rewrite_query_saved_at::<u32>(&old_path, 0);

    let args2 = Args {
        file: "Other.java".to_string(),
    };
    cache.store("type_of", &args2, &inputs, &43u32).unwrap();

    let report = prune_cache(
        &cache_dir,
        PrunePolicy {
            max_age_days: Some(1),
            ..PrunePolicy::default()
        },
    )
    .unwrap();

    assert!(!old_path.exists());

    let remaining: Vec<_> = std::fs::read_dir(&query_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("bin"))
        .collect();
    assert_eq!(remaining.len(), 1);
    assert!(cache_dir.metadata_path().is_file());
    assert!(report.deleted_files >= 1);
}

#[test]
fn dry_run_does_not_delete() {
    let (_tmp, cache_dir) = new_cache_dir();
    let artifact_name = store_ast(&cache_dir, "src/Foo.java", "class Foo {}");

    let dead = cache_dir.ast_dir().join("dead.ast");
    std::fs::write(&dead, b"orphan").unwrap();
    assert!(dead.is_file());

    let report = prune_cache(
        &cache_dir,
        PrunePolicy {
            dry_run: true,
            ..PrunePolicy::default()
        },
    )
    .unwrap();

    assert!(cache_dir.ast_dir().join(&artifact_name).exists());
    assert!(dead.exists());
    assert!(cache_dir.metadata_path().exists());
    assert_eq!(report.deleted_files, 0);
    assert!(report.would_delete_files >= 1);
}

#[test]
fn max_total_bytes_evicts_oldest_first() {
    let (_tmp, cache_dir) = new_cache_dir();
    let cache = DerivedArtifactCache::new(cache_dir.queries_dir());

    let mut inputs = BTreeMap::new();
    inputs.insert("Main.java".to_string(), Fingerprint::from_bytes("v1"));

    let args1 = Args {
        file: "Main.java".to_string(),
    };
    cache.store("type_of", &args1, &inputs, &1u32).unwrap();

    let query_dir = cache_dir.queries_dir().join("type_of");
    let old_path = std::fs::read_dir(&query_dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    rewrite_query_saved_at::<u32>(&old_path, 1);

    let args2 = Args {
        file: "Other.java".to_string(),
    };
    cache.store("type_of", &args2, &inputs, &2u32).unwrap();

    let mut entries: Vec<_> = std::fs::read_dir(&query_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    entries.sort();
    assert_eq!(entries.len(), 2);
    let new_path = entries.into_iter().find(|p| p != &old_path).unwrap();
    rewrite_query_saved_at::<u32>(&new_path, 2);

    let meta_size = std::fs::metadata(cache_dir.metadata_path()).unwrap().len()
        + std::fs::metadata(cache_dir.metadata_bin_path())
            .unwrap()
            .len();
    let new_size = std::fs::metadata(&new_path).unwrap().len();
    let limit = meta_size + new_size;

    prune_cache(
        &cache_dir,
        PrunePolicy {
            max_total_bytes: Some(limit),
            ..PrunePolicy::default()
        },
    )
    .unwrap();

    assert!(!old_path.exists());
    assert!(new_path.exists());
    assert!(cache_dir.metadata_path().is_file());
}

#[test]
fn corrupt_ast_metadata_is_best_effort() {
    let (_tmp, cache_dir) = new_cache_dir();

    let metadata_path = cache_dir.ast_dir().join("metadata.bin");
    std::fs::write(&metadata_path, b"not bincode").unwrap();
    let orphan = cache_dir.ast_dir().join("orphan.ast");
    std::fs::write(&orphan, b"data").unwrap();

    let report = prune_cache(&cache_dir, PrunePolicy::default()).unwrap();

    assert!(metadata_path.is_file());
    assert!(!orphan.exists());
    assert!(cache_dir.metadata_path().is_file());
    assert!(!report.errors.is_empty());
}

#[test]
fn indexes_keep_idx_files() {
    let (_tmp, cache_dir) = new_cache_dir();

    let idx = cache_dir.indexes_dir().join("symbols.idx");
    std::fs::write(&idx, b"index").unwrap();
    let legacy = cache_dir.indexes_dir().join("legacy.tmp");
    std::fs::write(&legacy, b"legacy").unwrap();

    let meta_size = std::fs::metadata(cache_dir.metadata_path()).unwrap().len()
        + std::fs::metadata(cache_dir.metadata_bin_path())
            .unwrap()
            .len();
    let idx_size = std::fs::metadata(&idx).unwrap().len();
    let limit = meta_size + idx_size;

    prune_cache(
        &cache_dir,
        PrunePolicy {
            max_total_bytes: Some(limit),
            ..PrunePolicy::default()
        },
    )
    .unwrap();

    assert!(idx.exists());
    assert!(!legacy.exists());
    assert!(cache_dir.metadata_path().exists());
}
