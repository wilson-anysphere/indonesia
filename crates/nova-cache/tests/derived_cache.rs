use nova_cache::{DerivedArtifactCache, Fingerprint};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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
    let temp = tempfile::tempdir().unwrap();
    let cache = DerivedArtifactCache::new(temp.path());

    let mut inputs = BTreeMap::new();
    inputs.insert("Main.java".to_string(), Fingerprint::from_bytes("v1"));

    let args = Args {
        file: "Main.java".to_string(),
    };
    let value = Value { answer: 42 };

    cache
        .store("type_of", &args, &inputs, &value)
        .expect("store");

    let loaded: Option<Value> = cache.load("type_of", &args, &inputs).expect("load");
    assert_eq!(loaded, Some(value));

    // Change inputs; should miss.
    inputs.insert("Main.java".to_string(), Fingerprint::from_bytes("v2"));
    let loaded: Option<Value> = cache.load("type_of", &args, &inputs).expect("load");
    assert_eq!(loaded, None);
}

#[test]
fn derived_artifact_cache_corruption_is_cache_miss() {
    let temp = tempfile::tempdir().unwrap();
    let cache = DerivedArtifactCache::new(temp.path());

    let mut inputs = BTreeMap::new();
    inputs.insert("Main.java".to_string(), Fingerprint::from_bytes("v1"));

    let args = Args {
        file: "Main.java".to_string(),
    };
    let value = Value { answer: 42 };

    cache
        .store("type_of", &args, &inputs, &value)
        .expect("store");

    let query_dir = temp.path().join("type_of");
    let entry_path = std::fs::read_dir(&query_dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    std::fs::write(&entry_path, b"not a valid bincode payload").unwrap();

    let loaded: Option<Value> = cache.load("type_of", &args, &inputs).expect("load");
    assert_eq!(loaded, None);
}

#[test]
fn derived_artifact_cache_oversized_payload_is_cache_miss() {
    let temp = tempfile::tempdir().unwrap();
    let cache = DerivedArtifactCache::new(temp.path());

    let mut inputs = BTreeMap::new();
    inputs.insert("Main.java".to_string(), Fingerprint::from_bytes("v1"));

    let args = Args {
        file: "Main.java".to_string(),
    };
    let value = Value { answer: 42 };

    cache
        .store("type_of", &args, &inputs, &value)
        .expect("store");

    let query_dir = temp.path().join("type_of");
    let entry_path = std::fs::read_dir(&query_dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();

    let file = std::fs::File::create(&entry_path).unwrap();
    file.set_len((nova_cache::BINCODE_PAYLOAD_LIMIT_BYTES + 1) as u64)
        .unwrap();

    let loaded: Option<Value> = cache.load("type_of", &args, &inputs).expect("load");
    assert_eq!(loaded, None);
}
