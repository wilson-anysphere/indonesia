use std::path::{Path, PathBuf};

use nova_remote_proto::{ShardId, ShardIndex};
use serde::{Deserialize, Serialize};

use crate::error::CacheError;
use crate::util::{
    atomic_write, bincode_deserialize, bincode_serialize, now_millis, read_file_limited,
};

pub const SHARD_INDEX_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Serialize)]
struct PersistedShardIndex<'a> {
    schema_version: u32,
    nova_version: String,
    protocol_version: u32,
    saved_at_millis: u64,
    index: &'a ShardIndex,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedShardIndexOwned {
    schema_version: u32,
    nova_version: String,
    protocol_version: u32,
    saved_at_millis: u64,
    index: ShardIndex,
}

pub fn shard_cache_path(cache_dir: &Path, shard_id: ShardId) -> PathBuf {
    cache_dir.join(format!("shard_{shard_id}.bin"))
}

pub fn save_shard_index(cache_dir: &Path, index: &ShardIndex) -> Result<(), CacheError> {
    let path = shard_cache_path(cache_dir, index.shard_id);
    let persisted = PersistedShardIndex {
        schema_version: SHARD_INDEX_SCHEMA_VERSION,
        nova_version: nova_core::NOVA_VERSION.to_string(),
        protocol_version: nova_remote_proto::PROTOCOL_VERSION,
        saved_at_millis: now_millis(),
        index,
    };
    let bytes = bincode_serialize(&persisted)?;
    atomic_write(&path, &bytes)
}

pub fn load_shard_index(
    cache_dir: &Path,
    shard_id: ShardId,
) -> Result<Option<ShardIndex>, CacheError> {
    let path = shard_cache_path(cache_dir, shard_id);
    let bytes = match read_file_limited(&path) {
        Some(bytes) => bytes,
        None => return Ok(None),
    };

    let persisted: PersistedShardIndexOwned = match bincode_deserialize(&bytes) {
        Ok(persisted) => persisted,
        Err(_) => return Ok(None),
    };

    if persisted.schema_version != SHARD_INDEX_SCHEMA_VERSION {
        return Ok(None);
    }
    if persisted.nova_version != nova_core::NOVA_VERSION {
        return Ok(None);
    }
    if persisted.protocol_version != nova_remote_proto::PROTOCOL_VERSION {
        return Ok(None);
    }
    if persisted.index.shard_id != shard_id {
        return Ok(None);
    }

    Ok(Some(persisted.index))
}
