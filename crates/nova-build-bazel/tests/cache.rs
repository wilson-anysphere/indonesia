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
