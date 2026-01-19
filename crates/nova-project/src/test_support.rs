use std::ffi::OsString;
use std::panic::Location;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, OnceLock};

static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[track_caller]
pub(crate) fn env_lock() -> MutexGuard<'static, ()> {
    match ENV_LOCK.get_or_init(|| Mutex::new(())).lock() {
        Ok(guard) => guard,
        Err(err) => {
            let loc = Location::caller();
            tracing::error!(
                target = "nova.project",
                file = loc.file(),
                line = loc.line(),
                column = loc.column(),
                error = %err,
                "env lock poisoned; continuing with recovered guard"
            );
            err.into_inner()
        }
    }
}

pub(crate) struct EnvVarGuard {
    key: &'static str,
    prior: Option<OsString>,
}

impl EnvVarGuard {
    pub(crate) fn set_path(key: &'static str, value: Option<&Path>) -> Self {
        let prior = std::env::var_os(key);
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
        Self { key, prior }
    }

    pub(crate) fn set_str(key: &'static str, value: Option<&str>) -> Self {
        let prior = std::env::var_os(key);
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
        Self { key, prior }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match self.prior.take() {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}
