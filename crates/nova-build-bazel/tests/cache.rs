use nova_build_bazel::{digest_file, BazelCache, CacheEntry, JavaCompileInfo};
use tempfile::tempdir;

#[test]
fn cache_is_keyed_by_query_hash_and_build_file_digests() {
    let dir = tempdir().unwrap();
    let build = dir.path().join("BUILD");
    std::fs::write(&build, "java_library(name = \"hello\")").unwrap();

    let digest = digest_file(&build).unwrap();
    let query_hash = blake3::hash(b"query-output");

    let mut cache = BazelCache::default();
    cache.insert(CacheEntry {
        target: "//:hello".to_string(),
        query_hash_hex: query_hash.to_hex().to_string(),
        build_files: vec![digest.clone()],
        info: JavaCompileInfo {
            classpath: vec!["a.jar".to_string()],
            ..JavaCompileInfo::default()
        },
    });

    assert!(cache
        .get("//:hello", query_hash, &[digest.clone()])
        .is_some());

    // Changing the BUILD file should invalidate the entry.
    std::fs::write(&build, "java_library(name = \"hello\", srcs = [])").unwrap();
    let new_digest = digest_file(&build).unwrap();
    assert!(cache.get("//:hello", query_hash, &[new_digest]).is_none());
}

#[test]
fn corrupt_cache_file_is_treated_as_empty() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("bazel.json");
    std::fs::write(&path, "not json").unwrap();

    let cache = BazelCache::load(&path).unwrap();
    assert_eq!(cache, BazelCache::default());
}

#[test]
fn cache_roundtrips_via_disk() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("bazel.json");

    let query_hash = blake3::hash(b"query-output");
    let mut cache = BazelCache::default();
    cache.insert(CacheEntry {
        target: "//:hello".to_string(),
        query_hash_hex: query_hash.to_hex().to_string(),
        build_files: Vec::new(),
        info: JavaCompileInfo {
            classpath: vec!["a.jar".to_string()],
            ..JavaCompileInfo::default()
        },
    });

    cache.save(&path).unwrap();

    let loaded = BazelCache::load(&path).unwrap();
    assert_eq!(loaded, cache);
}
