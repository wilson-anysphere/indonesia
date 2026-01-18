use crate::error::CacheError;
use fs2::FileExt as _;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

/// A filesystem-backed lock that is safe to share across multiple Nova processes.
///
/// The lock is released when the returned value is dropped.
#[derive(Debug)]
pub struct CacheLock {
    file: File,
    _path: PathBuf,
    // `fs2` file locks are process-scoped on Unix platforms (they don't exclude other threads in
    // the same process). Keep an in-process mutex guard to ensure mutual exclusion between
    // threads, while the file lock continues to provide cross-process coordination.
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl CacheLock {
    /// Acquire an exclusive lock on `path`, creating the lockfile if needed.
    ///
    /// This call blocks until the lock is available.
    pub fn lock_exclusive(path: &Path) -> Result<Self, CacheError> {
        let mutex = process_lock_for_path(path);
        let guard = lock_with_path(mutex, "CacheLock.lock_exclusive", path);

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        file.lock_exclusive()?;

        Ok(Self {
            file,
            _path: path.to_path_buf(),
            _guard: guard,
        })
    }
}

impl Drop for CacheLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn process_lock_for_path(path: &Path) -> &'static Mutex<()> {
    static PROCESS_LOCKS: OnceLock<Mutex<HashMap<PathBuf, &'static Mutex<()>>>> = OnceLock::new();
    let locks = PROCESS_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));

    let mut map = lock_with_path(locks, "CacheLock.process_lock_for_path.map", path);
    if let Some(existing) = map.get(path) {
        return existing;
    }

    let mutex: &'static Mutex<()> = Box::leak(Box::new(Mutex::new(())));
    map.insert(path.to_path_buf(), mutex);
    mutex
}

#[track_caller]
fn lock_with_path<'a, T>(
    mutex: &'a Mutex<T>,
    context: &'static str,
    path: &Path,
) -> MutexGuard<'a, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(err) => {
            let loc = std::panic::Location::caller();
            tracing::error!(
                target = "nova.cache",
                context,
                path = %path.display(),
                file = loc.file(),
                line = loc.line(),
                column = loc.column(),
                error = %err,
                "mutex poisoned; continuing with recovered guard"
            );
            err.into_inner()
        }
    }
}
