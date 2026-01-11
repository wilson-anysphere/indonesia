use crate::error::CacheError;
use bincode::Options;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
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

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), CacheError> {
    let Some(parent) = path.parent() else {
        return Err(std::io::Error::new(std::io::ErrorKind::Other, "path has no parent").into());
    };
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };

    fs::create_dir_all(parent)?;

    let (tmp_path, mut file) = open_unique_tmp_file(path, parent)?;
    if let Err(err) = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .and_then(|()| Ok(()))
    {
        drop(file);
        let _ = fs::remove_file(&tmp_path);
        return Err(CacheError::from(err));
    }
    drop(file);

    const MAX_RENAME_ATTEMPTS: usize = 1024;
    let rename_result = (|| -> io::Result<()> {
        let mut attempts = 0usize;
        loop {
            match fs::rename(&tmp_path, path) {
                Ok(()) => return Ok(()),
                Err(err)
                    if cfg!(windows)
                        && (err.kind() == io::ErrorKind::AlreadyExists || path.exists()) =>
                {
                    // On Windows, `rename` doesn't overwrite. Under concurrent writers,
                    // multiple `remove + rename` sequences can race; retry until we win.
                    match fs::remove_file(path) {
                        Ok(()) => {}
                        Err(remove_err) if remove_err.kind() == io::ErrorKind::NotFound => {}
                        Err(remove_err) => return Err(remove_err),
                    }

                    attempts += 1;
                    if attempts >= MAX_RENAME_ATTEMPTS {
                        return Err(err);
                    }

                    continue;
                }
                Err(err) => return Err(err),
            }
        }
    })();

    match rename_result {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = fs::remove_file(&tmp_path);
            Err(CacheError::from(err))
        }
    }
}

fn open_unique_tmp_file(dest: &Path, parent: &Path) -> io::Result<(PathBuf, fs::File)> {
    let file_name = dest.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::Other, "destination path has no file name")
    })?;
    let pid = std::process::id();

    loop {
        let counter = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut tmp_name = file_name.to_os_string();
        tmp_name.push(format!(".tmp.{pid}.{counter}"));
        let tmp_path = parent.join(tmp_name);

        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
        {
            Ok(file) => return Ok((tmp_path, file)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
}
