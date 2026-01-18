use bincode::Options;
use nova_cache::{DerivedArtifactCache, DerivedCachePolicy, Fingerprint};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

fn bincode_options() -> impl bincode::Options {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .with_no_limit()
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Args {
    file: String,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Value {
    answer: u32,
}

#[test]
fn derived_artifact_cache_roundtrip_and_invalidation() {
    let _guard = crate::test_lock();

    let temp = tempfile::tempdir().unwrap();
    let cache = DerivedArtifactCache::new(temp.path());
    let query_schema_version = 1;

    let mut inputs = BTreeMap::new();
    inputs.insert("Main.java".to_string(), Fingerprint::from_bytes("v1"));

    let args = Args {
        file: "Main.java".to_string(),
    };
    let value = Value { answer: 42 };

    cache
        .store("type_of", query_schema_version, &args, &inputs, &value)
        .expect("store");

    let loaded: Option<Value> = cache
        .load("type_of", query_schema_version, &args, &inputs)
        .expect("load");
    assert_eq!(loaded, Some(value));

    // Change inputs; should miss.
    inputs.insert("Main.java".to_string(), Fingerprint::from_bytes("v2"));
    let loaded: Option<Value> = cache
        .load("type_of", query_schema_version, &args, &inputs)
        .expect("load");
    assert_eq!(loaded, None);
}

#[test]
fn derived_artifact_cache_corruption_is_cache_miss() {
    let _guard = crate::test_lock();

    let temp = tempfile::tempdir().unwrap();
    let cache = DerivedArtifactCache::new(temp.path());
    let query_schema_version = 1;

    let mut inputs = BTreeMap::new();
    inputs.insert("Main.java".to_string(), Fingerprint::from_bytes("v1"));

    let args = Args {
        file: "Main.java".to_string(),
    };
    let value = Value { answer: 42 };

    cache
        .store("type_of", query_schema_version, &args, &inputs, &value)
        .expect("store");

    let query_dir = temp.path().join("type_of");
    let entry_path = std::fs::read_dir(&query_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().and_then(|e| e.to_str()) == Some("bin"))
        .expect("bin entry");
    std::fs::write(&entry_path, b"not a valid bincode payload").unwrap();

    let loaded: Option<Value> = cache
        .load("type_of", query_schema_version, &args, &inputs)
        .expect("load");
    assert_eq!(loaded, None);
    assert!(
        !entry_path.exists(),
        "expected corrupted cache entry to be deleted"
    );
}

#[test]
fn derived_artifact_cache_query_schema_version_is_part_of_key() {
    let _guard = crate::test_lock();

    let temp = tempfile::tempdir().unwrap();
    let cache = DerivedArtifactCache::new(temp.path());

    let mut inputs = BTreeMap::new();
    inputs.insert("Main.java".to_string(), Fingerprint::from_bytes("v1"));

    let args = Args {
        file: "Main.java".to_string(),
    };
    let value = Value { answer: 42 };

    cache
        .store("type_of", 1, &args, &inputs, &value)
        .expect("store");

    let loaded: Option<Value> = cache.load("type_of", 2, &args, &inputs).expect("load");
    assert_eq!(
        loaded, None,
        "changing query_schema_version should cause a clean cache miss"
    );
}

#[test]
fn derived_artifact_cache_persisted_query_schema_version_mismatch_is_cache_miss() {
    let _guard = crate::test_lock();

    use bincode::Options;

    let temp = tempfile::tempdir().unwrap();
    let cache = DerivedArtifactCache::new(temp.path());

    let mut inputs = BTreeMap::new();
    inputs.insert("Main.java".to_string(), Fingerprint::from_bytes("v1"));

    let args = Args {
        file: "Main.java".to_string(),
    };
    let value = Value { answer: 42 };

    cache.store("type_of", 1, &args, &inputs, &value).unwrap();

    let query_dir = temp.path().join("type_of");
    let entry_path = std::fs::read_dir(&query_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().and_then(|e| e.to_str()) == Some("bin"))
        .expect("bin entry");

    let bytes = std::fs::read(&entry_path).unwrap();
    let mut persisted: PersistedDerivedValueOwned<Value> =
        bincode_options().deserialize(&bytes).unwrap();
    persisted.query_schema_version = 2;
    let mutated = bincode_options().serialize(&persisted).unwrap();
    std::fs::write(&entry_path, mutated).unwrap();

    let loaded: Option<Value> = cache.load("type_of", 1, &args, &inputs).unwrap();
    assert_eq!(
        loaded, None,
        "query_schema_version is validated in the persisted payload"
    );
}

#[test]
fn derived_artifact_cache_oversized_payload_is_cache_miss() {
    let _guard = crate::test_lock();

    let temp = tempfile::tempdir().unwrap();
    let cache = DerivedArtifactCache::new(temp.path());
    let query_schema_version = 1;

    let mut inputs = BTreeMap::new();
    inputs.insert("Main.java".to_string(), Fingerprint::from_bytes("v1"));

    let args = Args {
        file: "Main.java".to_string(),
    };
    let value = Value { answer: 42 };

    cache
        .store("type_of", query_schema_version, &args, &inputs, &value)
        .expect("store");

    let query_dir = temp.path().join("type_of");
    let entry_path = std::fs::read_dir(&query_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().and_then(|e| e.to_str()) == Some("bin"))
        .expect("bin entry");

    let file = std::fs::File::create(&entry_path).unwrap();
    file.set_len((nova_cache::BINCODE_PAYLOAD_LIMIT_BYTES + 1) as u64)
        .unwrap();
    drop(file);

    let loaded: Option<Value> = cache
        .load("type_of", query_schema_version, &args, &inputs)
        .expect("load");
    assert_eq!(loaded, None);
    assert!(
        !entry_path.exists(),
        "expected oversized cache entry to be deleted"
    );
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct PersistedDerivedValueOwned<T> {
    schema_version: u32,
    query_schema_version: u32,
    nova_version: String,
    saved_at_millis: u64,
    query_name: String,
    key_fingerprint: Fingerprint,
    value: T,
}

#[test]
fn derived_artifact_cache_gc_respects_global_max_bytes() {
    let _guard = crate::test_lock();

    let temp = tempfile::tempdir().unwrap();
    let cache = DerivedArtifactCache::new(temp.path());
    let query_schema_version = 1;

    #[derive(Debug, Serialize)]
    struct BigValue {
        bytes: Vec<u8>,
    }

    let value = BigValue {
        bytes: vec![0u8; 1024],
    };

    let mut inputs = BTreeMap::new();
    inputs.insert("Main.java".to_string(), Fingerprint::from_bytes("v1"));

    for query in ["q1", "q2"] {
        for i in 0..10 {
            let args = Args {
                file: format!("{query}-{i}.java"),
            };
            cache
                .store(query, query_schema_version, &args, &inputs, &value)
                .unwrap();
        }
    }

    let before = cache.stats().unwrap();
    assert!(before.total_entries > 0);

    let policy = DerivedCachePolicy {
        max_bytes: 5 * 1024,
        max_age_ms: None,
        per_query_max_bytes: None,
    };
    let report = cache.gc(policy).unwrap();
    assert!(report.after.total_bytes <= policy.max_bytes);
    assert!(report.after.total_entries < report.before.total_entries);
}

#[test]
fn derived_artifact_cache_gc_respects_ttl() {
    let _guard = crate::test_lock();

    let temp = tempfile::tempdir().unwrap();
    let cache = DerivedArtifactCache::new(temp.path());
    let query_schema_version = 1;

    let mut inputs = BTreeMap::new();
    inputs.insert("Main.java".to_string(), Fingerprint::from_bytes("v1"));

    let args1 = Args {
        file: "Main.java".to_string(),
    };
    let args2 = Args {
        file: "Other.java".to_string(),
    };

    cache
        .store(
            "ttl_query",
            query_schema_version,
            &args1,
            &inputs,
            &Value { answer: 1 },
        )
        .unwrap();
    cache
        .store(
            "ttl_query",
            query_schema_version,
            &args2,
            &inputs,
            &Value { answer: 2 },
        )
        .unwrap();

    let query_dir = temp.path().join("ttl_query");
    let mut entries: Vec<_> = std::fs::read_dir(&query_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("bin"))
        .collect();
    entries.sort();
    assert_eq!(entries.len(), 2);

    // Patch one entry to look ancient; delete the index so `gc()` is forced to rebuild.
    let old_path = &entries[0];
    let bytes = std::fs::read(old_path).unwrap();
    let mut persisted: PersistedDerivedValueOwned<Value> =
        bincode_options().deserialize(&bytes).unwrap();
    persisted.saved_at_millis = 0;
    let bytes = bincode_options().serialize(&persisted).unwrap();
    std::fs::write(old_path, bytes).unwrap();

    // Ensure the other entry always looks fresh regardless of wall-clock skew
    // (e.g. if the system clock jumps forward between `store()` and `gc()`).
    let fresh_path = &entries[1];
    let bytes = std::fs::read(fresh_path).unwrap();
    let mut persisted: PersistedDerivedValueOwned<Value> =
        bincode_options().deserialize(&bytes).unwrap();
    persisted.saved_at_millis = u64::MAX;
    let bytes = bincode_options().serialize(&persisted).unwrap();
    std::fs::write(fresh_path, bytes).unwrap();

    let index_path = query_dir.join("index.json");
    if let Err(err) = std::fs::remove_file(&index_path) {
        if err.kind() != std::io::ErrorKind::NotFound {
            eprintln!(
                "failed to remove derived cache index file {}: {err}",
                index_path.display()
            );
        }
    }

    let policy = DerivedCachePolicy {
        max_bytes: u64::MAX,
        max_age_ms: Some(60_000),
        per_query_max_bytes: None,
    };
    cache.gc(policy).unwrap();

    let after = cache.stats().unwrap();
    let ttl_stats = after.per_query.get("ttl_query").unwrap();
    assert_eq!(ttl_stats.entries, 1);
}

#[test]
fn derived_artifact_cache_gc_survives_corrupt_index() {
    let _guard = crate::test_lock();

    let temp = tempfile::tempdir().unwrap();
    let cache = DerivedArtifactCache::new(temp.path());
    let query_schema_version = 1;

    let mut inputs = BTreeMap::new();
    inputs.insert("Main.java".to_string(), Fingerprint::from_bytes("v1"));

    for i in 0..5 {
        let args = Args {
            file: format!("Main{i}.java"),
        };
        cache
            .store(
                "corrupt_index",
                query_schema_version,
                &args,
                &inputs,
                &Value { answer: i },
            )
            .unwrap();
    }

    let query_dir = temp.path().join("corrupt_index");
    std::fs::write(query_dir.join("index.json"), b"not valid json").unwrap();

    let policy = DerivedCachePolicy {
        max_bytes: 0,
        max_age_ms: None,
        per_query_max_bytes: None,
    };
    let report = cache.gc(policy).unwrap();
    assert_eq!(report.after.total_entries, 0);
}
