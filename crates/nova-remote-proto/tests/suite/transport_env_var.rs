use nova_remote_proto::transport;

const MAX_MESSAGE_SIZE_ENV_VAR: &str = "NOVA_RPC_MAX_MESSAGE_SIZE";
const CHILD_MARKER_ENV_VAR: &str = "NOVA_RPC_TRANSPORT_ENV_VAR_CHILD";
const CHILD_TEST_FN: &str = "transport_respects_max_message_size_env_var_child";

// `transport::max_frame_size()` caches the env var on first use. With the test suite consolidated
// into a single binary, other tests may have already initialized the cache before this test runs.
//
// To keep this test deterministic regardless of test ordering, run the real assertions in a fresh
// subprocess where `NOVA_RPC_MAX_MESSAGE_SIZE` is set before startup.
#[test]
fn transport_respects_max_message_size_env_var() {
    let exe = std::env::current_exe().expect("current test executable path");

    // `--exact` matches against the libtest name, which is the module path *relative to the test
    // crate root* (it does not include the crate name). For this module, the name is:
    // `suite::transport_env_var::{CHILD_TEST_FN}`.
    let child_test = {
        let module_path = module_path!();
        let rel_module_path = module_path
            .split_once("::")
            .map(|(_, rest)| rest)
            .unwrap_or("");
        if rel_module_path.is_empty() {
            CHILD_TEST_FN.to_owned()
        } else {
            format!("{rel_module_path}::{CHILD_TEST_FN}")
        }
    };

    let output = std::process::Command::new(exe)
        .arg(&child_test)
        .arg("--exact")
        .env(CHILD_MARKER_ENV_VAR, "1")
        .env(MAX_MESSAGE_SIZE_ENV_VAR, "8")
        .output()
        .expect("spawn child test process");

    if !output.status.success() {
        panic!(
            "child test process failed (status: {}).\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn transport_respects_max_message_size_env_var_child() {
    if std::env::var(CHILD_MARKER_ENV_VAR).as_deref() != Ok("1") {
        return;
    }

    assert!(
        transport::encode_frame(&[0u8; 8]).is_ok(),
        "expected frame <= env var limit to be accepted"
    );
    assert!(
        transport::encode_frame(&[0u8; 9]).is_err(),
        "expected frame > env var limit to be rejected"
    );

    let mut bytes = Vec::from(9u32.to_le_bytes());
    bytes.extend_from_slice(&[0u8; 9]);
    assert!(
        transport::decode_frame(&bytes).is_err(),
        "expected incoming frame > env var limit to be rejected"
    );
}
