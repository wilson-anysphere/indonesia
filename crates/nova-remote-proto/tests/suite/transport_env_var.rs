use nova_remote_proto::transport;
use std::process::Command;

#[test]
fn transport_respects_max_message_size_env_var() {
    // `transport::max_frame_size()` caches the env var on first use. With the test suite
    // consolidated into a single binary, other tests may have already initialized the cache.
    //
    // Run this check in a fresh subprocess to preserve the original isolation.
    const CHILD_ENV: &str = "NOVA_REMOTE_PROTO_TRANSPORT_ENV_VAR_CHILD";

    if std::env::var_os(CHILD_ENV).is_some() {
        // The transport caches the env var on first use, so set it before touching any transport
        // APIs.
        std::env::set_var("NOVA_RPC_MAX_MESSAGE_SIZE", "8");

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
        return;
    }

    let exe = std::env::current_exe().expect("current_exe");
    let status = Command::new(exe)
        .env(CHILD_ENV, "1")
        .env("NOVA_RPC_MAX_MESSAGE_SIZE", "8")
        .arg("suite::transport_env_var::transport_respects_max_message_size_env_var")
        .arg("--exact")
        .status()
        .expect("spawn test subprocess");

    assert!(status.success(), "subprocess test failed");
}
