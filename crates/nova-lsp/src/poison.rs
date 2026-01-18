use std::panic::Location;
use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

#[track_caller]
pub(crate) fn lock<'a, T>(mutex: &'a Mutex<T>, context: &'static str) -> MutexGuard<'a, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(err) => {
            let loc = Location::caller();
            tracing::error!(
                target = "nova.lsp",
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

#[allow(dead_code)]
#[track_caller]
pub(crate) fn get_mut<'a, T>(mutex: &'a mut Mutex<T>, context: &'static str) -> &'a mut T {
    match mutex.get_mut() {
        Ok(value) => value,
        Err(err) => {
            let loc = Location::caller();
            tracing::error!(
                target = "nova.lsp",
                context,
                file = loc.file(),
                line = loc.line(),
                column = loc.column(),
                error = %err,
                "mutex poisoned on get_mut; continuing with recovered value"
            );
            err.into_inner()
        }
    }
}

#[allow(dead_code)]
#[track_caller]
pub(crate) fn rwlock_read<'a, T>(
    lock: &'a RwLock<T>,
    context: &'static str,
) -> RwLockReadGuard<'a, T> {
    match lock.read() {
        Ok(guard) => guard,
        Err(err) => {
            let loc = Location::caller();
            tracing::error!(
                target = "nova.lsp",
                context,
                file = loc.file(),
                line = loc.line(),
                column = loc.column(),
                error = %err,
                "rwlock poisoned on read; continuing with recovered guard"
            );
            err.into_inner()
        }
    }
}

#[allow(dead_code)]
#[track_caller]
pub(crate) fn rwlock_write<'a, T>(
    lock: &'a RwLock<T>,
    context: &'static str,
) -> RwLockWriteGuard<'a, T> {
    match lock.write() {
        Ok(guard) => guard,
        Err(err) => {
            let loc = Location::caller();
            tracing::error!(
                target = "nova.lsp",
                context,
                file = loc.file(),
                line = loc.line(),
                column = loc.column(),
                error = %err,
                "rwlock poisoned on write; continuing with recovered guard"
            );
            err.into_inner()
        }
    }
}

#[cfg(test)]
#[allow(dead_code)]
#[track_caller]
pub(crate) fn into_inner<T>(mutex: Mutex<T>, context: &'static str) -> T {
    match mutex.into_inner() {
        Ok(value) => value,
        Err(err) => {
            let loc = Location::caller();
            tracing::error!(
                target = "nova.lsp",
                context,
                file = loc.file(),
                line = loc.line(),
                column = loc.column(),
                error = %err,
                "mutex poisoned while taking inner value; continuing with recovered value"
            );
            err.into_inner()
        }
    }
}
