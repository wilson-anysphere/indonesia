use std::panic::Location;
use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

#[track_caller]
pub(crate) fn lock_std_mutex<'a, T>(
    mutex: &'a Mutex<T>,
    context: &'static str,
) -> MutexGuard<'a, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(err) => {
            let loc = Location::caller();
            tracing::error!(
                target = "nova.remote_rpc",
                context,
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

#[track_caller]
pub(crate) fn read_rwlock<'a, T>(
    lock: &'a RwLock<T>,
    context: &'static str,
) -> RwLockReadGuard<'a, T> {
    match lock.read() {
        Ok(guard) => guard,
        Err(err) => {
            let loc = Location::caller();
            tracing::error!(
                target = "nova.remote_rpc",
                context,
                file = loc.file(),
                line = loc.line(),
                column = loc.column(),
                error = %err,
                "rwlock poisoned; continuing with recovered guard"
            );
            err.into_inner()
        }
    }
}

#[track_caller]
pub(crate) fn write_rwlock<'a, T>(
    lock: &'a RwLock<T>,
    context: &'static str,
) -> RwLockWriteGuard<'a, T> {
    match lock.write() {
        Ok(guard) => guard,
        Err(err) => {
            let loc = Location::caller();
            tracing::error!(
                target = "nova.remote_rpc",
                context,
                file = loc.file(),
                line = loc.line(),
                column = loc.column(),
                error = %err,
                "rwlock poisoned; continuing with recovered guard"
            );
            err.into_inner()
        }
    }
}
