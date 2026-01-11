use nova_remote_proto::transport;

#[test]
fn transport_respects_max_message_size_env_var() {
    // The transport caches the env var on first use, so set it before touching any transport APIs.
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
}
