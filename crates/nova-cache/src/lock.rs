use crate::error::CacheError;
use fs2::FileExt as _;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

/// A filesystem-backed lock that is safe to share across multiple Nova processes.
///
/// The lock is released when the returned value is dropped.
#[derive(Debug)]
pub struct CacheLock {
    file: File,
    _path: PathBuf,
}

impl CacheLock {
    /// Acquire an exclusive lock on `path`, creating the lockfile if needed.
    ///
    /// This call blocks until the lock is available.
    pub fn lock_exclusive(path: &Path) -> Result<Self, CacheError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(path)?;
        file.lock_exclusive()?;

        Ok(Self {
            file,
            _path: path.to_path_buf(),
        })
    }
}

impl Drop for CacheLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}
