use std::path::{Path, PathBuf};

use nova_remote_proto::{ShardId, ShardIndex};

use crate::error::CacheError;
use crate::util::atomic_write;

pub fn shard_cache_path(cache_dir: &Path, shard_id: ShardId) -> PathBuf {
    cache_dir.join(format!("shard_{shard_id}.bin"))
}

pub fn save_shard_index(cache_dir: &Path, index: &ShardIndex) -> Result<(), CacheError> {
    let path = shard_cache_path(cache_dir, index.shard_id);
    let bytes = bincode::serialize(index)?;
    atomic_write(&path, &bytes)
}

pub fn load_shard_index(cache_dir: &Path, shard_id: ShardId) -> Result<Option<ShardIndex>, CacheError> {
    let path = shard_cache_path(cache_dir, shard_id);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(None),
    };

    let index = match bincode::deserialize(&bytes) {
        Ok(index) => index,
        Err(_) => return Ok(None),
    };

    Ok(Some(index))
}
