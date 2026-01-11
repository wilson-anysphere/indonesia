use nova_cache::{load_shard_index, save_shard_index, shard_cache_path};
use nova_remote_proto::{ShardId, ShardIndex, Symbol};

#[test]
fn shard_index_corruption_is_cache_miss() {
    let tmp = tempfile::tempdir().unwrap();
    let shard_id: ShardId = 7;

    let index = ShardIndex {
        shard_id,
        revision: 42,
        index_generation: 1,
        symbols: vec![Symbol {
            name: "Foo".to_string(),
            path: "src/Foo.java".to_string(),
        }],
    };

    save_shard_index(tmp.path(), &index).unwrap();
    let loaded = load_shard_index(tmp.path(), shard_id).unwrap().unwrap();
    assert_eq!(loaded, index);

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

    let index = ShardIndex {
        shard_id,
        revision: 42,
        index_generation: 1,
        symbols: vec![Symbol {
            name: "Foo".to_string(),
            path: "src/Foo.java".to_string(),
        }],
    };

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
