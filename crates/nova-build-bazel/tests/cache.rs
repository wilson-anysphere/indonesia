use nova_build_bazel::{digest_file, BazelCache, CacheEntry, JavaCompileInfo};
use std::sync::{Arc, Barrier};
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

#[test]
fn cache_save_is_safe_under_concurrent_writers() {
    let dir = tempdir().unwrap();
    let path = Arc::new(dir.path().join("bazel.json"));

    let query_hash_a = blake3::hash(b"query-output-a");
    let mut cache_a = BazelCache::default();
    cache_a.insert(CacheEntry {
        target: "//:a".to_string(),
        query_hash_hex: query_hash_a.to_hex().to_string(),
        build_files: Vec::new(),
        info: JavaCompileInfo {
            classpath: vec!["a.jar".to_string()],
            ..JavaCompileInfo::default()
        },
    });

    let query_hash_b = blake3::hash(b"query-output-b");
    let mut cache_b = BazelCache::default();
    cache_b.insert(CacheEntry {
        target: "//:b".to_string(),
        query_hash_hex: query_hash_b.to_hex().to_string(),
        build_files: Vec::new(),
        info: JavaCompileInfo {
            classpath: vec!["b.jar".to_string()],
            ..JavaCompileInfo::default()
        },
    });

    let expected_a = serde_json::to_string_pretty(&cache_a).unwrap();
    let expected_b = serde_json::to_string_pretty(&cache_b).unwrap();

    let cache_a = Arc::new(cache_a);
    let cache_b = Arc::new(cache_b);

    let threads = 8;
    let iterations = 32;
    let barrier = Arc::new(Barrier::new(threads));

    let mut handles = Vec::with_capacity(threads);
    for idx in 0..threads {
        let path = path.clone();
        let cache = if idx % 2 == 0 {
            cache_a.clone()
        } else {
            cache_b.clone()
        };
        let barrier = barrier.clone();

        handles.push(std::thread::spawn(move || {
            let mut error = None;
            for _ in 0..iterations {
                barrier.wait();
                if error.is_none() {
                    if let Err(err) = cache.save(path.as_path()) {
                        error = Some(err);
                    }
                }
            }
            if let Some(err) = error {
                Err(err)
            } else {
                Ok(())
            }
        }));
    }

    for handle in handles {
        handle.join().unwrap().unwrap();
    }

    let final_contents = std::fs::read_to_string(path.as_path()).unwrap();
    assert!(
        final_contents == expected_a || final_contents == expected_b,
        "final cache payload corrupted (len={})",
        final_contents.len()
    );
}
