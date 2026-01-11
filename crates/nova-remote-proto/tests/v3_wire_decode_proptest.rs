use nova_remote_proto::{
    v3::{decode_rpc_payload, decode_wire_frame, CompressionAlgo, WireFrame},
    MAX_MESSAGE_BYTES,
};
use proptest::prelude::*;

const MAX_FUZZ_INPUT_LEN: usize = 64 * 1024;

#[derive(Clone, Debug)]
enum TestCborValue {
    Unsigned(u64),
    Bytes(Vec<u8>),
    Text(String),
    Array(Vec<TestCborValue>),
    Map(Vec<(String, TestCborValue)>),
    Bool(bool),
    Null,
}

fn encode_uint(major: u8, n: u64, out: &mut Vec<u8>) {
    fn push_be<const N: usize>(v: u64, out: &mut Vec<u8>) {
        let bytes = v.to_be_bytes();
        out.extend_from_slice(&bytes[8 - N..]);
    }

    let major_bits = major << 5;
    match n {
        0..=23 => out.push(major_bits | (n as u8)),
        24..=0xFF => {
            out.push(major_bits | 24);
            out.push(n as u8);
        }
        0x100..=0xFFFF => {
            out.push(major_bits | 25);
            push_be::<2>(n, out);
        }
        0x1_0000..=0xFFFF_FFFF => {
            out.push(major_bits | 26);
            push_be::<4>(n, out);
        }
        _ => {
            out.push(major_bits | 27);
            push_be::<8>(n, out);
        }
    }
}

fn encode_map_header(len: u64, out: &mut Vec<u8>) {
    encode_uint(5, len, out);
}

fn encode_array_header(len: u64, out: &mut Vec<u8>) {
    encode_uint(4, len, out);
}

fn encode_bytes(bytes: &[u8], out: &mut Vec<u8>) {
    encode_uint(2, bytes.len() as u64, out);
    out.extend_from_slice(bytes);
}

fn encode_text(text: &str, out: &mut Vec<u8>) {
    encode_uint(3, text.len() as u64, out);
    out.extend_from_slice(text.as_bytes());
}

fn encode_value(value: &TestCborValue, out: &mut Vec<u8>) {
    match value {
        TestCborValue::Unsigned(n) => encode_uint(0, *n, out),
        TestCborValue::Bytes(bytes) => encode_bytes(bytes, out),
        TestCborValue::Text(text) => encode_text(text, out),
        TestCborValue::Array(items) => {
            encode_array_header(items.len() as u64, out);
            for item in items {
                encode_value(item, out);
            }
        }
        TestCborValue::Map(entries) => {
            encode_map_header(entries.len() as u64, out);
            for (k, v) in entries {
                encode_text(k, out);
                encode_value(v, out);
            }
        }
        TestCborValue::Bool(true) => out.push(0xf5),
        TestCborValue::Bool(false) => out.push(0xf4),
        TestCborValue::Null => out.push(0xf6),
    }
}

fn encode_packet_frame(
    extra_root: &[(String, TestCborValue)],
    extra_body: &[(String, TestCborValue)],
    data: &[u8],
) -> Vec<u8> {
    let mut out = Vec::new();
    encode_map_header((2 + extra_root.len()) as u64, &mut out);

    encode_text("type", &mut out);
    encode_text("packet", &mut out);

    encode_text("body", &mut out);
    encode_map_header((3 + extra_body.len()) as u64, &mut out);

    encode_text("id", &mut out);
    encode_uint(0, 1, &mut out);

    encode_text("compression", &mut out);
    encode_text("none", &mut out);

    encode_text("data", &mut out);
    encode_bytes(data, &mut out);

    for (k, v) in extra_body {
        encode_text(k, &mut out);
        encode_value(v, &mut out);
    }

    for (k, v) in extra_root {
        encode_text(k, &mut out);
        encode_value(v, &mut out);
    }

    out
}

fn encode_packet_frame_with_declared_data_len(data_len: u64) -> Vec<u8> {
    let mut out = Vec::new();
    encode_map_header(2, &mut out);

    encode_text("type", &mut out);
    encode_text("packet", &mut out);

    encode_text("body", &mut out);
    encode_map_header(3, &mut out);

    encode_text("id", &mut out);
    encode_uint(0, 1, &mut out);

    encode_text("compression", &mut out);
    encode_text("none", &mut out);

    encode_text("data", &mut out);
    // Malformed: declare a huge byte string but provide no bytes.
    encode_uint(2, data_len, &mut out);

    out
}

fn map_key_strategy(prefix: &'static str) -> BoxedStrategy<String> {
    proptest::string::string_regex("[a-z0-9_]{1,12}")
        .unwrap()
        .prop_map(move |suffix| format!("{prefix}{suffix}"))
        .boxed()
}

fn cbor_value_strategy() -> BoxedStrategy<TestCborValue> {
    let leaf = prop_oneof![
        any::<u64>().prop_map(TestCborValue::Unsigned),
        proptest::collection::vec(any::<u8>(), 0..16).prop_map(TestCborValue::Bytes),
        proptest::string::string_regex("[ -~]{0,16}")
            .unwrap()
            .prop_map(TestCborValue::Text),
        any::<bool>().prop_map(TestCborValue::Bool),
        Just(TestCborValue::Null),
    ];

    leaf.prop_recursive(3, 32, 8, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..4).prop_map(TestCborValue::Array),
            proptest::collection::vec((map_key_strategy("k_"), inner.clone()), 0..4)
                .prop_map(TestCborValue::Map),
        ]
    })
    .boxed()
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 64,
        .. ProptestConfig::default()
    })]

    #[test]
    fn v3_decode_never_panics_on_random_bytes(bytes in proptest::collection::vec(any::<u8>(), 0..=MAX_FUZZ_INPUT_LEN)) {
        let _ = decode_wire_frame(&bytes);
        let _ = decode_rpc_payload(&bytes);
    }

    #[test]
    fn v3_decode_rejects_oversize_embedded_byte_string_len(over in ((MAX_MESSAGE_BYTES as u64 + 1)..=u64::MAX)) {
        let bytes = encode_packet_frame_with_declared_data_len(over);
        prop_assert!(decode_wire_frame(&bytes).is_err());
    }

    #[test]
    fn v3_decode_handles_unknown_keys_in_cbor_maps(
        data in proptest::collection::vec(any::<u8>(), 0..128),
        extra_root in proptest::collection::vec((map_key_strategy("xroot_"), cbor_value_strategy()), 0..6),
        extra_body in proptest::collection::vec((map_key_strategy("xbody_"), cbor_value_strategy()), 0..6),
    ) {
        let bytes = encode_packet_frame(&extra_root, &extra_body, &data);
        let decoded = decode_wire_frame(&bytes).unwrap();

        match decoded {
            WireFrame::Packet { id, compression, data: decoded_data } => {
                prop_assert_eq!(id, 1);
                prop_assert_eq!(compression, CompressionAlgo::None);
                prop_assert_eq!(decoded_data, data);
            }
            other => prop_assert!(false, "expected packet frame, got {other:?}"),
        }
    }

    #[test]
    fn v3_decode_handles_wrong_types(
        case in 0u8..=4,
        data in proptest::collection::vec(any::<u8>(), 0..32),
    ) {
        let bytes = match case {
            // "type" is not a string.
            0 => {
                let mut out = Vec::new();
                encode_map_header(2, &mut out);
                encode_text("type", &mut out);
                encode_map_header(0, &mut out);
                encode_text("body", &mut out);
                encode_map_header(0, &mut out);
                out
            }
            // "body" is not a map.
            1 => {
                let mut out = Vec::new();
                encode_map_header(2, &mut out);
                encode_text("type", &mut out);
                encode_text("packet", &mut out);
                encode_text("body", &mut out);
                encode_uint(0, 1, &mut out);
                out
            }
            // body.id is wrong type.
            2 => {
                let mut out = Vec::new();
                encode_map_header(2, &mut out);
                encode_text("type", &mut out);
                encode_text("packet", &mut out);
                encode_text("body", &mut out);
                encode_map_header(3, &mut out);
                encode_text("id", &mut out);
                encode_map_header(0, &mut out);
                encode_text("compression", &mut out);
                encode_text("none", &mut out);
                encode_text("data", &mut out);
                encode_bytes(&data, &mut out);
                out
            }
            // body.compression is wrong type.
            3 => {
                let mut out = Vec::new();
                encode_map_header(2, &mut out);
                encode_text("type", &mut out);
                encode_text("packet", &mut out);
                encode_text("body", &mut out);
                encode_map_header(3, &mut out);
                encode_text("id", &mut out);
                encode_uint(0, 1, &mut out);
                encode_text("compression", &mut out);
                encode_map_header(0, &mut out);
                encode_text("data", &mut out);
                encode_bytes(&data, &mut out);
                out
            }
            // body.data is wrong type.
            _ => {
                let mut out = Vec::new();
                encode_map_header(2, &mut out);
                encode_text("type", &mut out);
                encode_text("packet", &mut out);
                encode_text("body", &mut out);
                encode_map_header(3, &mut out);
                encode_text("id", &mut out);
                encode_uint(0, 1, &mut out);
                encode_text("compression", &mut out);
                encode_text("none", &mut out);
                encode_text("data", &mut out);
                encode_map_header(0, &mut out);
                out
            }
        };

        prop_assert!(decode_wire_frame(&bytes).is_err());
    }

    #[test]
    fn v3_decode_handles_truncated_cbor(
        data in proptest::collection::vec(any::<u8>(), 0..64),
        extra_root in proptest::collection::vec((map_key_strategy("xroot_"), cbor_value_strategy()), 0..3),
        truncate_at in 0usize..512,
    ) {
        let bytes = encode_packet_frame(&extra_root, &[], &data);
        let truncate_at = truncate_at.min(bytes.len().saturating_sub(1));
        let truncated = &bytes[..truncate_at];
        prop_assert!(decode_wire_frame(truncated).is_err());
    }
}
