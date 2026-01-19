//! Shared environment variable test utilities.
//!
//! Many tests mutate process-wide environment variables. When integration tests are consolidated
//! into fewer binaries, those mutations can easily become flaky unless they are:
//! - scoped (save + restore), and
//! - serialized (environment mutation is process-global).
//!
//! This module provides a small RAII guard and a global lock to make that easy.

use std::ffi::{OsStr, OsString};
use std::panic::Location;
use std::sync::{Mutex, MutexGuard, OnceLock};

static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// Acquire a global lock to serialize process-wide environment mutations across tests.
///
/// Environment variables are shared process state; tests that change them should use this lock
/// to avoid race conditions in parallel test execution.
#[track_caller]
pub fn env_lock() -> MutexGuard<'static, ()> {
    match ENV_LOCK.get_or_init(|| Mutex::new(())).lock() {
        Ok(guard) => guard,
        Err(err) => {
            let loc = Location::caller();
            tracing::error!(
                target = "nova.test_utils",
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

/// RAII guard that sets/unsets an environment variable and restores the previous value on drop.
#[derive(Debug)]
pub struct EnvVarGuard {
    key: OsString,
    prev: Option<OsString>,
}

impl EnvVarGuard {
    /// Set an environment variable for the lifetime of the guard.
    ///
    /// Accepts common value types like `&str`, [`OsString`], and [`std::path::Path`].
    #[must_use]
    pub fn set(key: impl Into<OsString>, value: impl AsRef<OsStr>) -> Self {
        let key = key.into();
        let prev = std::env::var_os(&key);
        std::env::set_var(&key, value);
        Self { key, prev }
    }

    /// Set an environment variable using an owned [`OsString`] value.
    ///
    /// This is handy for non-UTF8 values on Unix.
    #[must_use]
    pub fn set_os(key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        let key = key.into();
        let prev = std::env::var_os(&key);
        std::env::set_var(&key, value.into());
        Self { key, prev }
    }

    /// Unset an environment variable for the lifetime of the guard.
    #[must_use]
    pub fn unset(key: impl Into<OsString>) -> Self {
        let key = key.into();
        let prev = std::env::var_os(&key);
        std::env::remove_var(&key);
        Self { key, prev }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match self.prev.take() {
            Some(prev) => std::env::set_var(&self.key, prev),
            None => std::env::remove_var(&self.key),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "NOVA_TEST_UTILS__ENV_GUARD_TEST";

    struct RestoreEnv {
        key: &'static str,
        prev: Option<OsString>,
    }

    impl RestoreEnv {
        fn capture(key: &'static str) -> Self {
            Self {
                key,
                prev: std::env::var_os(key),
            }
        }
    }

    impl Drop for RestoreEnv {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(prev) => std::env::set_var(self.key, prev),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[test]
    fn set_restores_prior_value() {
        let _lock = env_lock();
        let _restore = RestoreEnv::capture(KEY);

        std::env::set_var(KEY, "before");
        assert_eq!(std::env::var(KEY).unwrap(), "before");

        {
            let _guard = EnvVarGuard::set(KEY, "during");
            assert_eq!(std::env::var(KEY).unwrap(), "during");
        }

        assert_eq!(std::env::var(KEY).unwrap(), "before");
    }

    #[test]
    fn unset_restores_prior_value() {
        let _lock = env_lock();
        let _restore = RestoreEnv::capture(KEY);

        std::env::set_var(KEY, "before");
        assert_eq!(std::env::var(KEY).unwrap(), "before");

        {
            let _guard = EnvVarGuard::unset(KEY);
            assert!(std::env::var_os(KEY).is_none());
        }

        assert_eq!(std::env::var(KEY).unwrap(), "before");
    }

    #[cfg(unix)]
    #[test]
    fn supports_non_utf8_osstring_values_on_unix() {
        use std::os::unix::ffi::OsStringExt;

        let _lock = env_lock();
        let _restore = RestoreEnv::capture(KEY);

        // Value bytes are intentionally invalid UTF-8. (Also avoid NUL bytes, which are invalid in
        // env vars.)
        let value = OsString::from_vec(vec![0xF0, 0x28, 0x8C, 0x28]);

        {
            let _guard = EnvVarGuard::set_os(KEY, value.clone());
            let got = std::env::var_os(KEY).expect("env var should be set");
            assert_eq!(got.into_vec(), value.into_vec());
        }
    }

    #[test]
    fn env_lock_recovers_from_poisoning() {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = env_lock();
            panic!("intentionally poison env_lock");
        }));

        // Subsequent users should still be able to acquire the lock even though it's poisoned.
        let _guard = env_lock();
    }
}
