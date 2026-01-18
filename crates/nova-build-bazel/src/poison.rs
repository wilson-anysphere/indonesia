use std::panic::Location;
use std::sync::{Mutex, MutexGuard};

#[track_caller]
pub(crate) fn lock<'a, T>(mutex: &'a Mutex<T>, context: &'static str) -> MutexGuard<'a, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(err) => {
            let loc = Location::caller();
            tracing::error!(
              target = "nova.build.bazel",
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

#[cfg(any(test, feature = "bsp"))]
use std::sync::Condvar;

#[cfg(any(test, feature = "bsp"))]
#[track_caller]
pub(crate) fn wait<'a, T>(
    cv: &Condvar,
    guard: MutexGuard<'a, T>,
    context: &'static str,
) -> MutexGuard<'a, T> {
    match cv.wait(guard) {
        Ok(guard) => guard,
        Err(err) => {
            let loc = Location::caller();
            tracing::error!(
              target = "nova.build.bazel",
              context,
              file = loc.file(),
              line = loc.line(),
              column = loc.column(),
              error = %err,
              "mutex poisoned while waiting; continuing with recovered guard"
            );
            err.into_inner()
        }
    }
}
