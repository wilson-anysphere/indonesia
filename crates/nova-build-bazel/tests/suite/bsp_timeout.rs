#![cfg(feature = "bsp")]

use nova_build_bazel::{BspServerConfig, BspWorkspace};
use std::time::{Duration, Instant};
use tempfile::tempdir;

struct EnvVarGuard {
    key: &'static str,
    previous: Option<String>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let previous = std::env::var(key).ok();
        std::env::set_var(key, value);
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}

#[test]
fn bsp_connect_times_out_when_initialize_hangs() {
    let _lock = nova_build_bazel::test_support::env_lock();
    let _timeout_guard = EnvVarGuard::set("NOVA_BSP_CONNECT_TIMEOUT_MS", "50");

    let root = tempdir().unwrap();

    let fake_server_path = env!("CARGO_BIN_EXE_fake_bsp_server");
    let config = BspServerConfig {
        program: fake_server_path.to_string(),
        args: vec!["--hang-initialize".to_string()],
    };

    let start = Instant::now();
    let result = BspWorkspace::connect(root.path().to_path_buf(), config);
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_secs(2),
        "BSP connect should fail quickly; took {elapsed:?}"
    );
    let err = result.expect_err("connect should time out");
    let message = err.to_string();
    assert!(
        message.to_ascii_lowercase().contains("timed out"),
        "expected timeout error, got: {message}"
    );
}

