use nova_remote_proto::transport;
use std::ffi::{OsStr, OsString};

struct ScopedEnvVar {
    key: &'static str,
    previous: Option<OsString>,
}

impl ScopedEnvVar {
    fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
        let previous = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, previous }
    }
}

impl Drop for ScopedEnvVar {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}

#[test]
fn transport_respects_max_message_size_env_var() {
    let _lock = transport::__TRANSPORT_ENV_LOCK_FOR_TESTS.lock().unwrap();

    // `transport::max_frame_size()` caches the env var on first use. With the test suite
    // consolidated into a single binary, other tests may have already initialized the cache.
    //
    // Reset the cache after mutating the environment so this test is deterministic regardless of
    // test ordering.
    let env_guard = ScopedEnvVar::set("NOVA_RPC_MAX_MESSAGE_SIZE", "8");
    transport::__reset_max_frame_size_cache_for_tests();

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

    drop(env_guard);
    transport::__reset_max_frame_size_cache_for_tests();
}
