use std::path::{Path, PathBuf};

use nova_remote_proto::{ShardId, ShardIndex, MAX_SYMBOLS_PER_SHARD_INDEX, PROTOCOL_VERSION};
use serde::{Deserialize, Serialize};

use crate::error::CacheError;
use crate::util::{
    atomic_write, bincode_deserialize, bincode_serialize, remove_file_best_effort,
    BINCODE_PAYLOAD_LIMIT_BYTES,
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
    if bytes.starts_with(&SHARD_INDEX_CACHE_MAGIC) {
        if let Some(symbol_count) = fixint_symbols_len_in_versioned_wrapper(&bytes) {
            if symbol_count > MAX_SYMBOLS_PER_SHARD_INDEX as u64 {
                emit_cache_diagnostic(
                    shard_id,
                    &path,
                    format_args!(
                        "shard cache symbol count too large: {symbol_count} (limit {MAX_SYMBOLS_PER_SHARD_INDEX})"
                    ),
                );
                return Ok(None);
            }
        }

        let file: ShardIndexCacheFileOwned = match bincode_deserialize(&bytes) {
            Ok(file) => file,
            Err(err) => {
                emit_cache_diagnostic(
                    shard_id,
                    &path,
                    format_args!("failed to decode shard cache wrapper: {err}"),
                );
                return Ok(None);
            }
        };

        if file.magic != SHARD_INDEX_CACHE_MAGIC {
            emit_cache_diagnostic(shard_id, &path, format_args!("shard cache magic mismatch"));
            return Ok(None);
        }

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

    // 2) Legacy persisted wrapper format (no magic bytes).
    if let Some(header) = legacy_wrapper_header(&bytes) {
        if header.symbol_count > MAX_SYMBOLS_PER_SHARD_INDEX as u64 {
            emit_cache_diagnostic(
                shard_id,
                &path,
                format_args!(
                    "shard cache symbol count too large: {} (limit {MAX_SYMBOLS_PER_SHARD_INDEX})",
                    header.symbol_count
                ),
            );
            return Ok(None);
        }

        if header.protocol_version != PROTOCOL_VERSION {
            emit_cache_diagnostic(
                shard_id,
                &path,
                format_args!(
                    "incompatible protocol version in legacy wrapper: expected {PROTOCOL_VERSION}, found {}",
                    header.protocol_version
                ),
            );
            return Ok(None);
        }

        if header.index_shard_id != shard_id {
            emit_cache_diagnostic(
                shard_id,
                &path,
                format_args!(
                    "shard id mismatch in legacy cache payload: requested {shard_id}, found {}",
                    header.index_shard_id
                ),
            );
            return Ok(None);
        }

        let persisted: LegacyPersistedShardIndexOwned = match bincode_deserialize(&bytes) {
            Ok(persisted) => persisted,
            Err(err) => {
                emit_cache_diagnostic(
                    shard_id,
                    &path,
                    format_args!("failed to decode legacy shard cache wrapper: {err}"),
                );
                return Ok(None);
            }
        };

        return Ok(Some(persisted.index));
    }

    // 3) Legacy raw `ShardIndex` payload (no wrapper).
    if let Some(symbol_count) = fixint_symbols_len_in_raw_shard_index(&bytes) {
        if symbol_count > MAX_SYMBOLS_PER_SHARD_INDEX as u64 {
            emit_cache_diagnostic(
                shard_id,
                &path,
                format_args!(
                    "shard cache symbol count too large: {symbol_count} (limit {MAX_SYMBOLS_PER_SHARD_INDEX})"
                ),
            );
            return Ok(None);
        }
    }

    let index: ShardIndex = match bincode_deserialize(&bytes) {
        Ok(index) => index,
        Err(err) => {
            emit_cache_diagnostic(
                shard_id,
                &path,
                format_args!("failed to decode legacy shard cache: {err}"),
            );
            return Ok(None);
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

fn read_le_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let slice = bytes.get(offset..offset + 4)?;
    let mut out = [0u8; 4];
    out.copy_from_slice(slice);
    Some(u32::from_le_bytes(out))
}

fn read_le_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    let slice = bytes.get(offset..offset + 8)?;
    let mut out = [0u8; 8];
    out.copy_from_slice(slice);
    Some(u64::from_le_bytes(out))
}

fn fixint_symbols_len_in_versioned_wrapper(bytes: &[u8]) -> Option<u64> {
    // Wrapper header is fixed width (magic + two u32 fields), and we always write shard caches
    // using fixed-int bincode options.
    read_le_u64(bytes, 36)
}

#[derive(Debug)]
struct LegacyWrapperHeader {
    protocol_version: u32,
    index_shard_id: ShardId,
    symbol_count: u64,
}

fn legacy_wrapper_header(bytes: &[u8]) -> Option<LegacyWrapperHeader> {
    let schema_version = read_le_u32(bytes, 0)?;
    if schema_version != LEGACY_SHARD_INDEX_SCHEMA_VERSION {
        return None;
    }

    // Legacy wrapper uses fixed-int bincode and begins with `nova_version: String`, encoded as a
    // `u64` length prefix followed by UTF-8 bytes.
    let version_len = read_le_u64(bytes, 4)? as usize;
    let version_start = 12usize;
    let version_end = version_start.checked_add(version_len)?;
    let version_bytes = bytes.get(version_start..version_end)?;
    if version_bytes != nova_core::NOVA_VERSION.as_bytes() {
        return None;
    }

    let protocol_version = read_le_u32(bytes, version_end)?;

    // After the version string: protocol_version (u32) + saved_at_millis (u64) + ShardIndex header.
    let index_offset = version_end.checked_add(4 + 8)?;
    let index_shard_id: ShardId = read_le_u32(bytes, index_offset)?;
    let symbol_count = read_le_u64(bytes, index_offset.checked_add(20)?)?;

    Some(LegacyWrapperHeader {
        protocol_version,
        index_shard_id,
        symbol_count,
    })
}

fn fixint_symbols_len_in_raw_shard_index(bytes: &[u8]) -> Option<u64> {
    // ShardIndex header is fixed width (u32 + u64 + u64), followed by Vec length as u64.
    read_le_u64(bytes, 20)
}

fn read_shard_cache_bytes(path: &Path, shard_id: ShardId) -> Option<Vec<u8>> {
    // Avoid following symlinks out of the cache directory.
    let meta = match std::fs::symlink_metadata(path) {
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

    if meta.file_type().is_symlink() {
        remove_file_best_effort(path, "shard_index.symlink");
        emit_cache_diagnostic(
            shard_id,
            path,
            format_args!("shard cache path is a symlink"),
        );
        return None;
    }

    if !meta.is_file() {
        return None;
    }

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
    tracing::warn!(
        target: "nova.cache",
        shard_id,
        path = %path.display(),
        "{message}"
    );

    // Unit tests don't always install a subscriber; fall back to stderr to keep
    // failures diagnosable when `tracing` is effectively a no-op.
    #[cfg(test)]
    if !tracing::enabled!(tracing::Level::WARN) {
        eprintln!(
            "shard index cache miss (shard_id={shard_id}, path={}): {message}",
            path.display()
        );
    }
}
