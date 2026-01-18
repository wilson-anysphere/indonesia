use bincode::Options;
use nova_cache::{load_shard_index, save_shard_index, shard_cache_path};
use nova_remote_proto::{
    ShardId, ShardIndex, Symbol, MAX_SYMBOLS_PER_SHARD_INDEX, PROTOCOL_VERSION,
};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::sync::{Arc, Mutex};

const SHARD_INDEX_CACHE_MAGIC: [u8; 8] = *b"NOVASHRD";
const SHARD_INDEX_CACHE_FORMAT_VERSION: u32 = 1;
const LEGACY_SHARD_INDEX_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct ShardIndexCacheFile {
    magic: [u8; 8],
    cache_format_version: u32,
    protocol_version: u32,
    payload: ShardIndex,
}

#[derive(Debug, Serialize, Deserialize)]
struct LegacyPersistedShardIndex {
    schema_version: u32,
    nova_version: String,
    protocol_version: u32,
    saved_at_millis: u64,
    index: ShardIndex,
}

fn sample_index(shard_id: ShardId) -> ShardIndex {
    ShardIndex {
        shard_id,
        revision: 42,
        index_generation: 1,
        symbols: vec![Symbol {
            name: "Foo".to_string(),
            path: "src/Foo.java".to_string(),
            line: 0,
            column: 0,
        }],
    }
}

fn bincode_options() -> impl bincode::Options {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .with_limit(nova_cache::BINCODE_PAYLOAD_LIMIT_BYTES as u64)
}

#[test]
fn shard_index_roundtrip_new_format() {
    let _guard = crate::test_lock();

    let tmp = tempfile::tempdir().unwrap();
    let shard_id: ShardId = 7;
    let index = sample_index(shard_id);

    save_shard_index(tmp.path(), &index).unwrap();

    // New format should be self-describing and include versioning information.
    let path = shard_cache_path(tmp.path(), shard_id);
    let bytes = std::fs::read(&path).unwrap();
    let file: ShardIndexCacheFile = bincode_options().deserialize(&bytes).unwrap();
    assert_eq!(file.magic, SHARD_INDEX_CACHE_MAGIC);
    assert_eq!(file.cache_format_version, SHARD_INDEX_CACHE_FORMAT_VERSION);
    assert_eq!(file.protocol_version, PROTOCOL_VERSION);
    assert_eq!(file.payload, index);

    let loaded = load_shard_index(tmp.path(), shard_id).unwrap().unwrap();
    assert_eq!(loaded, index);
}

#[test]
fn shard_index_loads_legacy_raw_format() {
    let _guard = crate::test_lock();

    let tmp = tempfile::tempdir().unwrap();
    let shard_id: ShardId = 7;
    let index = sample_index(shard_id);

    // Legacy format is a raw bincode-encoded `ShardIndex` with no wrapper.
    let path = shard_cache_path(tmp.path(), shard_id);
    let bytes = bincode::serialize(&index).unwrap();
    std::fs::write(&path, bytes).unwrap();

    let loaded = load_shard_index(tmp.path(), shard_id).unwrap().unwrap();
    assert_eq!(loaded, index);
}

#[test]
fn shard_index_loads_legacy_persisted_wrapper() {
    let _guard = crate::test_lock();

    let tmp = tempfile::tempdir().unwrap();
    let shard_id: ShardId = 7;
    let index = sample_index(shard_id);

    // Legacy format written before the cache was explicitly self describing.
    let persisted = LegacyPersistedShardIndex {
        schema_version: LEGACY_SHARD_INDEX_SCHEMA_VERSION,
        nova_version: nova_core::NOVA_VERSION.to_string(),
        protocol_version: PROTOCOL_VERSION,
        saved_at_millis: 0,
        index: index.clone(),
    };

    let bytes = bincode_options().serialize(&persisted).unwrap();
    let path = shard_cache_path(tmp.path(), shard_id);
    std::fs::write(&path, bytes).unwrap();

    let loaded = load_shard_index(tmp.path(), shard_id).unwrap().unwrap();
    assert_eq!(loaded, index);
}

#[derive(Clone)]
struct BufferWriter(Arc<Mutex<Vec<u8>>>);

struct BufferGuard(Arc<Mutex<Vec<u8>>>);

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for BufferWriter {
    type Writer = BufferGuard;

    fn make_writer(&'a self) -> Self::Writer {
        BufferGuard(self.0.clone())
    }
}

impl Write for BufferGuard {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn shard_index_protocol_mismatch_is_cache_miss_and_logs() {
    let _guard = crate::test_lock();

    let tmp = tempfile::tempdir().unwrap();
    let shard_id: ShardId = 7;
    let index = sample_index(shard_id);

    let file = ShardIndexCacheFile {
        magic: SHARD_INDEX_CACHE_MAGIC,
        cache_format_version: SHARD_INDEX_CACHE_FORMAT_VERSION,
        protocol_version: PROTOCOL_VERSION + 1,
        payload: index,
    };

    let bytes = bincode_options().serialize(&file).unwrap();
    let path = shard_cache_path(tmp.path(), shard_id);
    std::fs::write(&path, bytes).unwrap();

    let buf = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .without_time()
        .with_writer(BufferWriter(buf.clone()))
        .finish();

    tracing::subscriber::with_default(subscriber, || {
        let loaded = load_shard_index(tmp.path(), shard_id).unwrap();
        assert!(loaded.is_none());
    });

    let output =
        String::from_utf8(buf.lock().unwrap_or_else(|err| err.into_inner()).clone()).unwrap();
    assert!(output.contains("incompatible protocol version"), "{output}");
    assert!(output.contains("shard_id=7"), "{output}");
    assert!(output.contains("shard_7.bin"), "{output}");
}

#[test]
fn shard_index_corruption_is_cache_miss() {
    let _guard = crate::test_lock();

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
    let _guard = crate::test_lock();

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

#[cfg(unix)]
#[test]
fn shard_index_symlink_is_cache_miss() {
    let _guard = crate::test_lock();

    use std::os::unix::fs::symlink;

    let tmp = tempfile::tempdir().unwrap();
    let shard_id: ShardId = 7;
    let index = sample_index(shard_id);

    save_shard_index(tmp.path(), &index).unwrap();
    let path = shard_cache_path(tmp.path(), shard_id);
    let bytes = std::fs::read(&path).unwrap();

    let target = tmp.path().join("real_shard.bin");
    std::fs::write(&target, bytes).unwrap();
    std::fs::remove_file(&path).unwrap();
    symlink(&target, &path).unwrap();

    let loaded = load_shard_index(tmp.path(), shard_id).unwrap();
    assert!(loaded.is_none());
    assert!(!path.exists());
}

#[test]
fn shard_index_wrapper_with_too_many_symbols_is_cache_miss() {
    let _guard = crate::test_lock();

    let tmp = tempfile::tempdir().unwrap();
    let shard_id: ShardId = 7;

    // Write a truncated wrapper payload with an absurd symbol count. `load_shard_index` should
    // reject it without attempting to allocate a giant `Vec<Symbol>`.
    let too_many = (MAX_SYMBOLS_PER_SHARD_INDEX as u64) + 1;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&SHARD_INDEX_CACHE_MAGIC);
    bytes.extend_from_slice(&SHARD_INDEX_CACHE_FORMAT_VERSION.to_le_bytes());
    bytes.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
    bytes.extend_from_slice(&shard_id.to_le_bytes());
    bytes.extend_from_slice(&0u64.to_le_bytes()); // revision
    bytes.extend_from_slice(&0u64.to_le_bytes()); // index_generation
    bytes.extend_from_slice(&too_many.to_le_bytes());

    let path = shard_cache_path(tmp.path(), shard_id);
    std::fs::write(&path, bytes).unwrap();

    let loaded = load_shard_index(tmp.path(), shard_id).unwrap();
    assert!(loaded.is_none());
}

#[test]
fn shard_index_raw_with_too_many_symbols_is_cache_miss() {
    let _guard = crate::test_lock();

    let tmp = tempfile::tempdir().unwrap();
    let shard_id: ShardId = 7;

    let too_many = (MAX_SYMBOLS_PER_SHARD_INDEX as u64) + 1;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&shard_id.to_le_bytes());
    bytes.extend_from_slice(&0u64.to_le_bytes()); // revision
    bytes.extend_from_slice(&0u64.to_le_bytes()); // index_generation
    bytes.extend_from_slice(&too_many.to_le_bytes());

    let path = shard_cache_path(tmp.path(), shard_id);
    std::fs::write(&path, bytes).unwrap();

    let loaded = load_shard_index(tmp.path(), shard_id).unwrap();
    assert!(loaded.is_none());
}
