use std::path::{Path, PathBuf};

use bincode::Options;
use nova_remote_proto::{ShardId, ShardIndex, PROTOCOL_VERSION};
use serde::{Deserialize, Serialize};

use crate::error::CacheError;
use crate::util::{
    atomic_write, bincode_deserialize, bincode_serialize, BINCODE_PAYLOAD_LIMIT_BYTES,
};

const SHARD_INDEX_CACHE_MAGIC: [u8; 8] = *b"NOVASHRD";
const SHARD_INDEX_CACHE_FORMAT_VERSION: u32 = 1;

/// Legacy wrapper used before shard index caches were explicitly self-describing.
///
/// Kept for migration so upgrades don't drop all caches at once.
const LEGACY_SHARD_INDEX_SCHEMA_VERSION: u32 = 1;

/// Current on-disk shard index cache format (versioned + self describing).
#[derive(Debug, Serialize)]
struct ShardIndexCacheFile<'a> {
    magic: [u8; 8],
    cache_format_version: u32,
    protocol_version: u32,
    payload: &'a ShardIndex,
}

#[derive(Debug, Serialize, Deserialize)]
struct ShardIndexCacheFileOwned {
    magic: [u8; 8],
    cache_format_version: u32,
    protocol_version: u32,
    payload: ShardIndex,
}

/// Legacy wrapper format written before the cache gained magic bytes.
#[derive(Debug, Serialize, Deserialize)]
struct LegacyPersistedShardIndexOwned {
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
    let file = ShardIndexCacheFile {
        magic: SHARD_INDEX_CACHE_MAGIC,
        cache_format_version: SHARD_INDEX_CACHE_FORMAT_VERSION,
        protocol_version: PROTOCOL_VERSION,
        payload: index,
    };
    let bytes = bincode_serialize(&file)?;
    atomic_write(&path, &bytes)
}

pub fn load_shard_index(
    cache_dir: &Path,
    shard_id: ShardId,
) -> Result<Option<ShardIndex>, CacheError> {
    let path = shard_cache_path(cache_dir, shard_id);
    let Some(bytes) = read_shard_cache_bytes(&path, shard_id) else {
        return Ok(None);
    };

    // 1) Current versioned wrapper format.
    match bincode_deserialize::<ShardIndexCacheFileOwned>(&bytes) {
        Ok(file) if file.magic == SHARD_INDEX_CACHE_MAGIC => {
            if file.cache_format_version != SHARD_INDEX_CACHE_FORMAT_VERSION {
                emit_cache_diagnostic(
                    shard_id,
                    &path,
                    format_args!(
                        "unsupported shard cache format version: expected {SHARD_INDEX_CACHE_FORMAT_VERSION}, found {}",
                        file.cache_format_version
                    ),
                );
                return Ok(None);
            }

            if file.protocol_version != PROTOCOL_VERSION {
                emit_cache_diagnostic(
                    shard_id,
                    &path,
                    format_args!(
                        "incompatible protocol version: expected {PROTOCOL_VERSION}, found {}",
                        file.protocol_version
                    ),
                );
                return Ok(None);
            }

            if file.payload.shard_id != shard_id {
                emit_cache_diagnostic(
                    shard_id,
                    &path,
                    format_args!(
                        "shard id mismatch in cache payload: requested {shard_id}, found {}",
                        file.payload.shard_id
                    ),
                );
                return Ok(None);
            }

            return Ok(Some(file.payload));
        }
        Ok(_) | Err(_) => {}
    }

    // 2) Legacy persisted wrapper format (no magic bytes).
    match bincode_deserialize::<LegacyPersistedShardIndexOwned>(&bytes) {
        Ok(persisted)
            if persisted.schema_version == LEGACY_SHARD_INDEX_SCHEMA_VERSION
                && persisted.nova_version == nova_core::NOVA_VERSION =>
        {
            if persisted.protocol_version != PROTOCOL_VERSION {
                emit_cache_diagnostic(
                    shard_id,
                    &path,
                    format_args!(
                        "incompatible protocol version in legacy wrapper: expected {PROTOCOL_VERSION}, found {}",
                        persisted.protocol_version
                    ),
                );
                return Ok(None);
            }

            if persisted.index.shard_id != shard_id {
                emit_cache_diagnostic(
                    shard_id,
                    &path,
                    format_args!(
                        "shard id mismatch in legacy cache payload: requested {shard_id}, found {}",
                        persisted.index.shard_id
                    ),
                );
                return Ok(None);
            }

            return Ok(Some(persisted.index));
        }
        Ok(_) | Err(_) => {}
    }

    // 3) Legacy raw `ShardIndex` payload (no wrapper).
    let index = match bincode_deserialize::<ShardIndex>(&bytes) {
        Ok(index) => index,
        Err(fixint_err) => {
            let legacy = bincode::DefaultOptions::new()
                .with_little_endian()
                .with_limit(BINCODE_PAYLOAD_LIMIT_BYTES as u64)
                .deserialize::<ShardIndex>(&bytes);

            match legacy {
                Ok(index) => index,
                Err(default_err) => {
                    emit_cache_diagnostic(
                        shard_id,
                        &path,
                        format_args!(
                            "failed to decode shard cache (fixint error: {fixint_err}; legacy error: {default_err})"
                        ),
                    );
                    return Ok(None);
                }
            }
        }
    };

    if index.shard_id != shard_id {
        emit_cache_diagnostic(
            shard_id,
            &path,
            format_args!(
                "shard id mismatch in legacy cache payload: requested {shard_id}, found {}",
                index.shard_id
            ),
        );
        return Ok(None);
    }

    Ok(Some(index))
}

fn read_shard_cache_bytes(path: &Path, shard_id: ShardId) -> Option<Vec<u8>> {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            emit_cache_diagnostic(
                shard_id,
                path,
                format_args!("failed to stat shard cache file: {err}"),
            );
            return None;
        }
    };

    if meta.len() > BINCODE_PAYLOAD_LIMIT_BYTES as u64 {
        emit_cache_diagnostic(
            shard_id,
            path,
            format_args!(
                "shard cache file too large: {} bytes (limit {} bytes)",
                meta.len(),
                BINCODE_PAYLOAD_LIMIT_BYTES
            ),
        );
        return None;
    }

    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            emit_cache_diagnostic(
                shard_id,
                path,
                format_args!("failed to read shard cache file: {err}"),
            );
            return None;
        }
    };

    if bytes.len() > BINCODE_PAYLOAD_LIMIT_BYTES {
        emit_cache_diagnostic(
            shard_id,
            path,
            format_args!(
                "shard cache file too large: {} bytes (limit {} bytes)",
                bytes.len(),
                BINCODE_PAYLOAD_LIMIT_BYTES
            ),
        );
        return None;
    }

    Some(bytes)
}

fn emit_cache_diagnostic(shard_id: ShardId, path: &Path, message: std::fmt::Arguments<'_>) {
    // Prefer tracing when it's configured, but fall back to stderr in binaries
    // (like `nova-worker`) that don't install a subscriber.
    if tracing::enabled!(tracing::Level::WARN) {
        tracing::warn!(
            target: "nova.cache",
            shard_id,
            path = %path.display(),
            "{message}"
        );
    } else {
        eprintln!(
            "shard index cache miss (shard_id={shard_id}, path={}): {message}",
            path.display()
        );
    }
}
