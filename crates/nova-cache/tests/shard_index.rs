use bincode::Options;
use nova_cache::{load_shard_index, save_shard_index, shard_cache_path};
use nova_remote_proto::{ShardId, ShardIndex, Symbol};
use serde::{Deserialize, Serialize};

fn sample_index(shard_id: ShardId) -> ShardIndex {
    ShardIndex {
        shard_id,
        revision: 42,
        index_generation: 1,
        symbols: vec![Symbol {
            name: "Foo".to_string(),
            path: "src/Foo.java".to_string(),
        }],
    }
}

fn bincode_options() -> impl bincode::Options {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .with_limit(nova_cache::BINCODE_PAYLOAD_LIMIT_BYTES as u64)
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedShardIndex {
    schema_version: u32,
    nova_version: String,
    protocol_version: u32,
    saved_at_millis: u64,
    index: ShardIndex,
}

#[test]
fn shard_index_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let shard_id: ShardId = 7;
    let index = sample_index(shard_id);

    save_shard_index(tmp.path(), &index).unwrap();
    let loaded = load_shard_index(tmp.path(), shard_id).unwrap().unwrap();
    assert_eq!(loaded, index);
}

#[test]
fn shard_index_protocol_version_mismatch_is_cache_miss() {
    let tmp = tempfile::tempdir().unwrap();
    let shard_id: ShardId = 7;
    let index = sample_index(shard_id);

    save_shard_index(tmp.path(), &index).unwrap();

    // Rewrite the persisted wrapper with a mismatched protocol version.
    let path = shard_cache_path(tmp.path(), shard_id);
    let bytes = std::fs::read(&path).unwrap();
    let mut persisted: PersistedShardIndex = bincode_options().deserialize(&bytes).unwrap();
    persisted.protocol_version = nova_remote_proto::PROTOCOL_VERSION + 1;
    let bytes = bincode_options().serialize(&persisted).unwrap();
    std::fs::write(&path, bytes).unwrap();

    assert!(load_shard_index(tmp.path(), shard_id).unwrap().is_none());
}

#[test]
fn shard_index_corruption_is_cache_miss() {
    let tmp = tempfile::tempdir().unwrap();
    let shard_id: ShardId = 7;
    let index = sample_index(shard_id);

    save_shard_index(tmp.path(), &index).unwrap();

    // Corrupt the on-disk payload; loading should fall back to a cache miss.
    let path = shard_cache_path(tmp.path(), shard_id);
    std::fs::write(&path, b"not bincode").unwrap();
    let loaded = load_shard_index(tmp.path(), shard_id).unwrap();
    assert!(loaded.is_none());
}

#[test]
fn shard_index_oversized_payload_is_cache_miss() {
    let tmp = tempfile::tempdir().unwrap();
    let shard_id: ShardId = 7;
    let index = sample_index(shard_id);

    save_shard_index(tmp.path(), &index).unwrap();
    let loaded = load_shard_index(tmp.path(), shard_id).unwrap().unwrap();
    assert_eq!(loaded, index);

    let path = shard_cache_path(tmp.path(), shard_id);
    let file = std::fs::File::create(&path).unwrap();
    file.set_len((nova_cache::BINCODE_PAYLOAD_LIMIT_BYTES + 1) as u64)
        .unwrap();

    let loaded = load_shard_index(tmp.path(), shard_id).unwrap();
    assert!(loaded.is_none());
}
