use nova_remote_proto::transport;
use nova_remote_proto::{RpcMessage, PROTOCOL_VERSION};

#[test]
fn rpc_v2_worker_hello_golden_vector() {
    let _lock = transport::__TRANSPORT_ENV_LOCK_FOR_TESTS
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    assert_eq!(
        PROTOCOL_VERSION, 4,
        "golden vectors are versioned; update testdata when bumping PROTOCOL_VERSION"
    );

    let expected = RpcMessage::WorkerHello {
        shard_id: 1,
        auth_token: Some("test-token".into()),
        has_cached_index: true,
    };

    let bytes: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/testdata/rpc_v2_hello.bin"
    ));

    let decoded = transport::decode_framed_message(bytes).expect("decode golden frame");
    assert_eq!(decoded, expected);

    let reencoded = transport::encode_framed_message(&expected).expect("re-encode golden frame");
    assert_eq!(reencoded.as_slice(), bytes);
}
