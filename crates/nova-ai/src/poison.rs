use std::panic::Location;
use std::sync::{Mutex, MutexGuard};

#[cfg(feature = "local-llm")]
use std::sync::OnceLock;

#[track_caller]
pub(crate) fn lock<'a, T>(mutex: &'a Mutex<T>, context: &'static str) -> MutexGuard<'a, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(err) => {
            let loc = Location::caller();
            tracing::error!(
                target = "nova.ai",
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

#[cfg(feature = "local-llm")]
#[track_caller]
pub(crate) fn lock_once<'a, T>(
    mutex: &'a Mutex<T>,
    context: &'static str,
    logged: &OnceLock<()>,
) -> MutexGuard<'a, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(err) => {
            if logged.set(()).is_ok() {
                let loc = Location::caller();
                tracing::error!(
                    target = "nova.ai",
                    context,
                    file = loc.file(),
                    line = loc.line(),
                    column = loc.column(),
                    error = %err,
                    "mutex poisoned; continuing with recovered guard"
                );
            }
            err.into_inner()
        }
    }
}
