use crate::error::CacheError;
use bincode::Options;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
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
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as u64,
        Err(err) => {
            // This should be extremely rare (system clock set before 1970). Avoid spamming logs
            // in any hot call sites by logging at most once.
            static REPORTED: OnceLock<()> = OnceLock::new();
            if REPORTED.set(()).is_ok() {
                tracing::debug!(
                    target = "nova.cache",
                    error = %err,
                    "system time is before unix epoch; using 0 for now_millis"
                );
            }
            0
        }
    }
}

pub(crate) fn bincode_options() -> impl bincode::Options + Copy {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
}

pub(crate) fn bincode_options_limited() -> impl bincode::Options + Copy {
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
    // Avoid following symlinks out of the cache directory.
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) => {
            // Cache misses are expected; only log unexpected filesystem errors.
            if err.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!(
                    target = "nova.cache",
                    path = %path.display(),
                    error = %err,
                    "failed to stat cache file"
                );
            }
            return None;
        }
    };
    if meta.file_type().is_symlink() || !meta.is_file() {
        remove_file_best_effort(path, "read_file_limited.invalid_type");
        return None;
    }

    if meta.len() > BINCODE_PAYLOAD_LIMIT_BYTES as u64 {
        remove_file_best_effort(path, "read_file_limited.oversize_meta");
        return None;
    }

    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!(
                    target = "nova.cache",
                    path = %path.display(),
                    error = %err,
                    "failed to read cache file"
                );
            }
            return None;
        }
    };
    if bytes.len() > BINCODE_PAYLOAD_LIMIT_BYTES {
        remove_file_best_effort(path, "read_file_limited.oversize_read");
        return None;
    }

    Some(bytes)
}

pub(crate) fn remove_file_best_effort(path: &Path, reason: &'static str) -> bool {
    match std::fs::remove_file(path) {
        Ok(()) => true,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => true,
        Err(err) => {
            tracing::debug!(
                target = "nova.cache",
                path = %path.display(),
                reason,
                error = %err,
                "failed to remove cache file"
            );
            false
        }
    }
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), CacheError> {
    atomic_write_with(path, |file| {
        file.write_all(bytes)?;
        Ok(())
    })
}

pub(crate) fn atomic_write_with(
    path: &Path,
    write: impl FnOnce(&mut fs::File) -> Result<(), CacheError>,
) -> Result<(), CacheError> {
    let Some(parent) = path.parent() else {
        return Err(io::Error::other("path has no parent").into());
    };
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };

    fs::create_dir_all(parent)?;

    let (tmp_path, mut file) = open_unique_tmp_file(path, parent)?;
    let write_result = (|| -> Result<(), CacheError> {
        write(&mut file)?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(err) = write_result {
        drop(file);
        if let Err(remove_err) = fs::remove_file(&tmp_path) {
            if remove_err.kind() != io::ErrorKind::NotFound {
                tracing::debug!(
                    target = "nova.cache",
                    path = %tmp_path.display(),
                    error = %remove_err,
                    "failed to remove temporary file after write failure"
                );
            }
        }
        return Err(err);
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
        Ok(()) => {
            sync_dir_best_effort(parent, "atomic_write_with.sync_parent_dir");
            Ok(())
        }
        Err(err) => {
            if let Err(remove_err) = fs::remove_file(&tmp_path) {
                if remove_err.kind() != io::ErrorKind::NotFound {
                    tracing::debug!(
                        target = "nova.cache",
                        path = %tmp_path.display(),
                        error = %remove_err,
                        "failed to remove temporary file after rename failure"
                    );
                }
            }
            Err(CacheError::from(err))
        }
    }
}

#[track_caller]
fn sync_dir_best_effort(dir: &Path, reason: &'static str) {
    #[cfg(unix)]
    static SYNC_DIR_ERROR_LOGGED: OnceLock<()> = OnceLock::new();

    #[cfg(unix)]
    {
        match fs::File::open(dir).and_then(|dir| dir.sync_all()) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => {
                if SYNC_DIR_ERROR_LOGGED.set(()).is_ok() {
                    let loc = std::panic::Location::caller();
                    tracing::debug!(
                        target = "nova.cache",
                        dir = %dir.display(),
                        reason,
                        file = loc.file(),
                        line = loc.line(),
                        column = loc.column(),
                        error = %err,
                        "failed to sync directory (best effort)"
                    );
                }
            }
        }
    }

    #[cfg(not(unix))]
    let _ = (dir, reason);
}

fn open_unique_tmp_file(dest: &Path, parent: &Path) -> io::Result<(PathBuf, fs::File)> {
    let file_name = dest
        .file_name()
        .ok_or_else(|| io::Error::other("destination path has no file name"))?;
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
