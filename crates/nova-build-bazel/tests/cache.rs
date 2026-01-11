use nova_build_bazel::{digest_file, BazelCache, CacheEntry, CompileInfoProvider, JavaCompileInfo};
use std::sync::{Arc, Barrier};
use tempfile::tempdir;

#[test]
fn cache_is_keyed_by_expr_version_and_file_digests() {
    let dir = tempdir().unwrap();
    let build = dir.path().join("BUILD");
    std::fs::write(&build, "java_library(name = \"hello\")").unwrap();

    let digest = digest_file(&build).unwrap();
    let expr_version_hex = "expr-v1".to_string();

    let mut cache = BazelCache::default();
    cache.insert(CacheEntry {
        target: "//:hello".to_string(),
        expr_version_hex: expr_version_hex.clone(),
        files: vec![digest.clone()],
        provider: CompileInfoProvider::Aquery,
        info: JavaCompileInfo {
            classpath: vec!["a.jar".to_string()],
            ..JavaCompileInfo::default()
        },
    });

    assert!(cache
        .get("//:hello", &expr_version_hex, CompileInfoProvider::Aquery)
        .is_some());

    // Changing the BUILD file should invalidate the entry.
    std::fs::write(&build, "java_library(name = \"hello\", srcs = [])").unwrap();
    assert!(cache
        .get("//:hello", &expr_version_hex, CompileInfoProvider::Aquery)
        .is_none());
}

#[test]
fn cache_does_not_mix_compile_info_providers() {
    let expr_version_hex = "expr-v1".to_string();
    let mut cache = BazelCache::default();
    cache.insert(CacheEntry {
        target: "//:hello".to_string(),
        expr_version_hex: expr_version_hex.clone(),
        files: Vec::new(),
        provider: CompileInfoProvider::Aquery,
        info: JavaCompileInfo {
            classpath: vec!["a.jar".to_string()],
            ..JavaCompileInfo::default()
        },
    });

    cache.insert(CacheEntry {
        target: "//:hello".to_string(),
        expr_version_hex: expr_version_hex.clone(),
        files: Vec::new(),
        provider: CompileInfoProvider::Bsp,
        info: JavaCompileInfo {
            classpath: vec!["b.jar".to_string()],
            ..JavaCompileInfo::default()
        },
    });

    let aquery = cache
        .get("//:hello", &expr_version_hex, CompileInfoProvider::Aquery)
        .unwrap();
    assert_eq!(aquery.info.classpath, vec!["a.jar".to_string()]);

    let bsp = cache
        .get("//:hello", &expr_version_hex, CompileInfoProvider::Bsp)
        .unwrap();
    assert_eq!(bsp.info.classpath, vec!["b.jar".to_string()]);
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

    let expr_version_hex = "expr-v1".to_string();
    let mut cache = BazelCache::default();
    cache.insert(CacheEntry {
        target: "//:hello".to_string(),
        expr_version_hex: expr_version_hex.clone(),
        files: Vec::new(),
        provider: CompileInfoProvider::Aquery,
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
fn cache_deserializes_legacy_compile_info_fields() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("bazel.json");

    // Historical cache payloads used `enable_preview` and `output_directory` field names inside the
    // `JavaCompileInfo` struct. Ensure we can still read those caches.
    let json = r#"
{
  "entries": {
    "//:hello": {
      "target": "//:hello",
      "expr_version_hex": "expr-v1",
      "files": [],
      "info": {
        "classpath": ["a.jar"],
        "enable_preview": true,
        "output_directory": "out/classes"
      }
    }
  }
}
"#;
    std::fs::write(&path, json).unwrap();

    let cache = BazelCache::load(&path).unwrap();
    let entry = cache.get("//:hello", "expr-v1").expect("missing cache entry");
    assert!(entry.info.preview);
    assert_eq!(entry.info.output_dir.as_deref(), Some("out/classes"));
}

#[test]
fn invalidate_changed_files_drops_matching_entries() {
    let dir = tempdir().unwrap();
    let build = dir.path().join("BUILD");
    std::fs::write(&build, "java_library(name = \"hello\")").unwrap();
    let digest = digest_file(&build).unwrap();
    let expr_version_hex = "expr-v1".to_string();

    let mut cache = BazelCache::default();
    cache.insert(CacheEntry {
        target: "//:hello".to_string(),
        expr_version_hex: expr_version_hex.clone(),
        files: vec![digest],
        provider: CompileInfoProvider::Aquery,
        info: JavaCompileInfo {
            classpath: vec!["a.jar".to_string()],
            ..JavaCompileInfo::default()
        },
    });

    assert!(cache
        .get("//:hello", &expr_version_hex, CompileInfoProvider::Aquery)
        .is_some());

    cache.invalidate_changed_files(&[dir.path().join("unrelated.txt")]);
    assert!(cache
        .get("//:hello", &expr_version_hex, CompileInfoProvider::Aquery)
        .is_some());

    cache.invalidate_changed_files(&[build]);
    assert!(cache
        .get("//:hello", &expr_version_hex, CompileInfoProvider::Aquery)
        .is_none());
}

#[test]
fn cache_save_is_safe_under_concurrent_writers() {
    let dir = tempdir().unwrap();
    let path = Arc::new(dir.path().join("bazel.json"));

    let mut cache_a = BazelCache::default();
    cache_a.insert(CacheEntry {
        target: "//:a".to_string(),
        expr_version_hex: "expr-a".to_string(),
        files: Vec::new(),
        provider: CompileInfoProvider::Aquery,
        info: JavaCompileInfo {
            classpath: vec!["a.jar".to_string()],
            ..JavaCompileInfo::default()
        },
    });

    let mut cache_b = BazelCache::default();
    cache_b.insert(CacheEntry {
        target: "//:b".to_string(),
        expr_version_hex: "expr-b".to_string(),
        files: Vec::new(),
        provider: CompileInfoProvider::Aquery,
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
