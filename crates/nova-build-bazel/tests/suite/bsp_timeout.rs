use nova_build_bazel::{test_support::EnvVarGuard, BspServerConfig, BspWorkspace};
use std::time::{Duration, Instant};
use tempfile::tempdir;

#[test]
fn bsp_connect_times_out_when_initialize_hangs() {
    let _lock = nova_build_bazel::test_support::env_lock();
    let _program_guard = EnvVarGuard::remove("NOVA_BSP_PROGRAM");
    let _args_guard = EnvVarGuard::remove("NOVA_BSP_ARGS");
    let _timeout_guard = EnvVarGuard::set("NOVA_BSP_CONNECT_TIMEOUT_MS", Some("50"));

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

#[test]
fn bsp_request_times_out_when_server_hangs() {
    let _lock = nova_build_bazel::test_support::env_lock();
    let _program_guard = EnvVarGuard::remove("NOVA_BSP_PROGRAM");
    let _args_guard = EnvVarGuard::remove("NOVA_BSP_ARGS");
    // Keep the handshake on the default timeout to avoid flakes; only shorten the request timeout
    // for the method under test.
    let _connect_guard = EnvVarGuard::set("NOVA_BSP_REQUEST_TIMEOUT_MS", None);

    let root = tempdir().unwrap();

    let fake_server_path = env!("CARGO_BIN_EXE_fake_bsp_server");
    let config = BspServerConfig {
        program: fake_server_path.to_string(),
        args: vec![
            "--hang-method".to_string(),
            "workspace/buildTargets".to_string(),
        ],
    };

    let mut workspace = BspWorkspace::connect(root.path().to_path_buf(), config).unwrap();

    let _request_timeout_guard = EnvVarGuard::set("NOVA_BSP_REQUEST_TIMEOUT_MS", Some("50"));

    let start = Instant::now();
    let result = workspace.build_targets();
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_secs(2),
        "BSP request should fail quickly; took {elapsed:?}"
    );
    let err = result.expect_err("buildTargets should time out");
    let message = err.to_string();
    assert!(
        message.to_ascii_lowercase().contains("timed out"),
        "expected timeout error, got: {message}"
    );
    assert!(
        message.contains("workspace/buildTargets"),
        "expected method name in error, got: {message}"
    );
}

#[test]
fn bsp_connect_timeout_is_handshake_only() {
    let _lock = nova_build_bazel::test_support::env_lock();
    let _program_guard = EnvVarGuard::remove("NOVA_BSP_PROGRAM");
    let _args_guard = EnvVarGuard::remove("NOVA_BSP_ARGS");
    let _connect_timeout_guard = EnvVarGuard::set("NOVA_BSP_CONNECT_TIMEOUT_MS", Some("200"));
    let _request_timeout_guard = EnvVarGuard::set("NOVA_BSP_REQUEST_TIMEOUT_MS", Some("1000"));

    let root = tempdir().unwrap();

    let fake_server_path = env!("CARGO_BIN_EXE_fake_bsp_server");
    let config = BspServerConfig {
        program: fake_server_path.to_string(),
        args: Vec::new(),
    };

    let mut workspace = BspWorkspace::connect(root.path().to_path_buf(), config).unwrap();

    // If the connect timeout was (incorrectly) applied as a lifetime timeout, this would
    // likely kill the server before the next request.
    std::thread::sleep(Duration::from_millis(300));

    let err = workspace
        .build_targets()
        .expect_err("fake server should not implement buildTargets");
    let message = err.to_string();

    // We expect a JSON-RPC method-not-found error, not a transport error like "closed the connection".
    assert!(
        message.contains("method not found") || message.contains("method not supported"),
        "expected method-not-found error, got: {message}"
    );
}
