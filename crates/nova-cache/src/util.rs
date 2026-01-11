use crate::error::CacheError;
use bincode::Options;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Hard upper bound for any bincode-encoded cache payload we will attempt to
/// deserialize from disk.
///
/// Cache corruption should degrade to a cache miss, not an out-of-memory crash.
/// This cap is intentionally conservative: it's large enough for typical AST,
/// derived query, and shard index payloads, but small enough to prevent
/// corrupted length prefixes from requesting enormous allocations.
pub const BINCODE_PAYLOAD_LIMIT_BYTES: usize = 64 * 1024 * 1024;

pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub(crate) fn bincode_options() -> impl bincode::Options {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
}

pub(crate) fn bincode_options_limited() -> impl bincode::Options {
    bincode_options().with_limit(BINCODE_PAYLOAD_LIMIT_BYTES as u64)
}

pub(crate) fn bincode_serialize<T: Serialize>(value: &T) -> Result<Vec<u8>, CacheError> {
    Ok(bincode_options().serialize(value)?)
}

pub(crate) fn bincode_deserialize<T: for<'de> Deserialize<'de>>(
    bytes: &[u8],
) -> Result<T, CacheError> {
    Ok(bincode_options_limited().deserialize(bytes)?)
}

pub(crate) fn read_file_limited(path: &Path) -> Option<Vec<u8>> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > BINCODE_PAYLOAD_LIMIT_BYTES as u64 {
        return None;
    }

    let bytes = std::fs::read(path).ok()?;
    if bytes.len() > BINCODE_PAYLOAD_LIMIT_BYTES {
        return None;
    }

    Some(bytes)
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), CacheError> {
    let Some(parent) = path.parent() else {
        return Err(std::io::Error::new(std::io::ErrorKind::Other, "path has no parent").into());
    };

    std::fs::create_dir_all(parent)?;

    let tmp_path = path.with_extension("tmp");
    {
        let mut file = std::fs::File::create(&tmp_path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }

    match std::fs::rename(&tmp_path, path) {
        Ok(()) => Ok(()),
        Err(_err) if path.exists() => {
            // On Windows, rename doesn't overwrite. Try remove + rename.
            std::fs::remove_file(path)?;
            std::fs::rename(&tmp_path, path).map_err(CacheError::from)
        }
        Err(err) => Err(CacheError::from(err)),
    }
}
