use std::panic::Location;
use std::sync::{Mutex, MutexGuard};

#[track_caller]
pub(super) fn lock<'a, T>(mutex: &'a Mutex<T>, context: &'static str) -> MutexGuard<'a, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(err) => {
            let loc = Location::caller();
            tracing::error!(
              target = "nova.jdwp",
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
