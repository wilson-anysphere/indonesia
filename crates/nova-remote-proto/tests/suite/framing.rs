use nova_remote_proto::transport;
use nova_remote_proto::{RpcMessage, MAX_FRAME_BYTES};

#[test]
fn decode_framed_message_rejects_truncated_payload() {
    let _lock = transport::__TRANSPORT_ENV_LOCK_FOR_TESTS
        .lock()
        .expect("__TRANSPORT_ENV_LOCK_FOR_TESTS mutex poisoned");
    let frame = transport::encode_framed_message(&RpcMessage::Ack).unwrap();
    let truncated = &frame[..frame.len() - 1];
    assert!(transport::decode_framed_message(truncated).is_err());
}

#[test]
fn decode_framed_message_rejects_trailing_bytes() {
    let _lock = transport::__TRANSPORT_ENV_LOCK_FOR_TESTS
        .lock()
        .expect("__TRANSPORT_ENV_LOCK_FOR_TESTS mutex poisoned");
    let mut frame = transport::encode_framed_message(&RpcMessage::Ack).unwrap();
    frame.push(0);
    assert!(transport::decode_framed_message(&frame).is_err());
}

#[test]
fn decode_framed_message_rejects_invalid_payload() {
    let _lock = transport::__TRANSPORT_ENV_LOCK_FOR_TESTS
        .lock()
        .expect("__TRANSPORT_ENV_LOCK_FOR_TESTS mutex poisoned");
    let frame = transport::encode_frame(&[0xff]).unwrap();
    assert!(transport::decode_framed_message(&frame).is_err());
}

#[test]
fn decode_framed_message_rejects_oversized_len_prefix() {
    let _lock = transport::__TRANSPORT_ENV_LOCK_FOR_TESTS
        .lock()
        .expect("__TRANSPORT_ENV_LOCK_FOR_TESTS mutex poisoned");
    let oversized_len: u32 = (MAX_FRAME_BYTES as u32).saturating_add(1);
    let bytes = oversized_len.to_le_bytes();
    assert!(transport::decode_framed_message(&bytes).is_err());
}
