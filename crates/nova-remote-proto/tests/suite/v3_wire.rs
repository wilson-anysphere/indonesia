use nova_remote_proto::v3::{
    decode_rpc_payload, decode_wire_frame, encode_rpc_payload, encode_wire_frame, CachedIndexInfo,
    Capabilities, CompressionAlgo, ProtocolVersion, Request, RpcPayload, SupportedVersions,
    WireFrame,
};
use nova_remote_proto::{FileText, ShardIndex, Symbol, WorkerStats};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

#[test]
fn hello_and_welcome_roundtrip() {
    let hello = nova_remote_proto::v3::WorkerHello {
        shard_id: 7,
        auth_token: Some("secret".into()),
        supported_versions: SupportedVersions {
            min: ProtocolVersion::CURRENT,
            max: ProtocolVersion::CURRENT,
        },
        capabilities: Capabilities {
            max_frame_len: 1024 * 1024,
            max_packet_len: 1024 * 1024,
            supported_compression: vec![CompressionAlgo::None, CompressionAlgo::Zstd],
            supports_cancel: true,
            supports_chunking: true,
        },
        cached_index_info: Some(CachedIndexInfo {
            revision: 42,
            index_generation: 9,
            symbol_count: 1,
        }),
        worker_build: Some("nova-worker test".into()),
    };

    let frame = WireFrame::Hello(hello);
    let bytes = encode_wire_frame(&frame).unwrap();
    let decoded = decode_wire_frame(&bytes).unwrap();
    assert_eq!(decoded, frame);

    let welcome = nova_remote_proto::v3::RouterWelcome {
        worker_id: 123,
        shard_id: 7,
        revision: 42,
        chosen_version: ProtocolVersion::CURRENT,
        chosen_capabilities: Capabilities::default(),
    };

    let frame = WireFrame::Welcome(welcome);
    let bytes = encode_wire_frame(&frame).unwrap();
    let decoded = decode_wire_frame(&bytes).unwrap();
    assert_eq!(decoded, frame);
}

#[test]
fn packet_frame_roundtrip() {
    let payload = RpcPayload::Request(Request::IndexShard {
        revision: 5,
        files: vec![FileText {
            path: "src/Main.java".into(),
            text: "class Main {}".into(),
        }],
    });
    let payload_bytes = encode_rpc_payload(&payload).unwrap();

    let frame = WireFrame::Packet {
        id: 99,
        compression: CompressionAlgo::None,
        data: payload_bytes,
    };

    let frame_bytes = encode_wire_frame(&frame).unwrap();
    let decoded = decode_wire_frame(&frame_bytes).unwrap();
    assert_eq!(decoded, frame);

    let WireFrame::Packet { data, .. } = decoded else {
        panic!("expected packet frame");
    };
    let decoded_payload = decode_rpc_payload(&data).unwrap();
    assert_eq!(decoded_payload, payload);
}

#[test]
fn decoding_ignores_unknown_fields_in_structs() {
    let index = ShardIndex {
        shard_id: 1,
        revision: 1,
        index_generation: 1,
        symbols: vec![Symbol {
            name: "Foo".into(),
            path: "Foo.java".into(),
            line: 0,
            column: 0,
        }],
    };

    let stats = WorkerStats {
        shard_id: 1,
        revision: 1,
        index_generation: 1,
        file_count: 1,
    };

    let hello = nova_remote_proto::v3::WorkerHello {
        shard_id: 1,
        auth_token: None,
        supported_versions: SupportedVersions {
            min: ProtocolVersion::CURRENT,
            max: ProtocolVersion::CURRENT,
        },
        capabilities: Capabilities::default(),
        cached_index_info: Some(CachedIndexInfo::from_index(&index)),
        worker_build: None,
    };

    let frame = WireFrame::Hello(hello);
    let bytes = encode_wire_frame(&frame).unwrap();

    let mut value: serde_cbor::Value = serde_cbor::from_slice(&bytes).unwrap();

    // Insert an extra field into the nested `WorkerHello` map.
    // This simulates forward-compatible field additions.
    if let serde_cbor::Value::Map(ref mut map) = value {
        let mut inserted = false;
        for (key, val) in map.iter_mut() {
            if matches!(key, serde_cbor::Value::Text(s) if s == "body") {
                let serde_cbor::Value::Map(ref mut body) = val else {
                    panic!("expected `body` to be a CBOR map");
                };
                body.insert(
                    serde_cbor::Value::Text("future_field".into()),
                    serde_cbor::Value::Text("ignored".into()),
                );
                inserted = true;
            }
        }
        assert!(inserted, "expected `body` key");
    } else {
        panic!("expected encoded WireFrame to be a CBOR map");
    }

    let bytes = serde_cbor::to_vec(&value).unwrap();
    let decoded = decode_wire_frame(&bytes).unwrap();
    assert_eq!(decoded, frame);

    // Sanity check that other shared structs still deserialize correctly.
    let payload = RpcPayload::Response(nova_remote_proto::v3::RpcResult::Ok {
        value: nova_remote_proto::v3::Response::WorkerStats(stats),
    });
    let bytes = encode_rpc_payload(&payload).unwrap();
    let decoded_payload = decode_rpc_payload(&bytes).unwrap();
    assert_eq!(decoded_payload, payload);
}

#[test]
fn decoding_unknown_enum_variants_does_not_fail() {
    // Unknown compression algorithm should map to `CompressionAlgo::Unknown`.
    let mut body = BTreeMap::new();
    body.insert(
        serde_cbor::Value::Text("id".into()),
        serde_cbor::Value::Integer(1.into()),
    );
    body.insert(
        serde_cbor::Value::Text("compression".into()),
        serde_cbor::Value::Text("lz4".into()),
    );
    body.insert(
        serde_cbor::Value::Text("data".into()),
        serde_cbor::Value::Bytes(vec![1, 2, 3]),
    );
    let mut frame = BTreeMap::new();
    frame.insert(
        serde_cbor::Value::Text("type".into()),
        serde_cbor::Value::Text("packet".into()),
    );
    frame.insert(
        serde_cbor::Value::Text("body".into()),
        serde_cbor::Value::Map(body),
    );
    let v = serde_cbor::Value::Map(frame);

    let bytes = serde_cbor::to_vec(&v).unwrap();
    let decoded = decode_wire_frame(&bytes).unwrap();
    assert_eq!(
        decoded,
        WireFrame::Packet {
            id: 1,
            compression: CompressionAlgo::Unknown,
            data: vec![1, 2, 3],
        }
    );

    // Unknown frame types should map to `WireFrame::Unknown`.
    let mut frame = BTreeMap::new();
    frame.insert(
        serde_cbor::Value::Text("type".into()),
        serde_cbor::Value::Text("mystery".into()),
    );
    frame.insert(
        serde_cbor::Value::Text("body".into()),
        serde_cbor::Value::Null,
    );
    let v = serde_cbor::Value::Map(frame);
    let bytes = serde_cbor::to_vec(&v).unwrap();
    let decoded = decode_wire_frame(&bytes).unwrap();
    assert!(matches!(decoded, WireFrame::Unknown));

    // Unknown payload types should map to `RpcPayload::Unknown`.
    let mut payload = BTreeMap::new();
    payload.insert(
        serde_cbor::Value::Text("type".into()),
        serde_cbor::Value::Text("mystery".into()),
    );
    payload.insert(
        serde_cbor::Value::Text("body".into()),
        serde_cbor::Value::Null,
    );
    let v = serde_cbor::Value::Map(payload);
    let bytes = serde_cbor::to_vec(&v).unwrap();
    let decoded = decode_rpc_payload(&bytes).unwrap();
    assert!(matches!(decoded, RpcPayload::Unknown));
}

#[test]
fn decoding_rejects_allocation_bombs() {
    // `WireFrame::Packet` with `data` declared as a 4GiB byte string (but no bytes follow).
    // This must fail without attempting a giant allocation.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&[
        0xa2, // map(2)
        0x64, b't', b'y', b'p', b'e', // "type"
        0x66, b'p', b'a', b'c', b'k', b'e', b't', // "packet"
        0x64, b'b', b'o', b'd', b'y', // "body"
        0xa3, // map(3)
        0x62, b'i', b'd', // "id"
        0x01, // 1
        0x6b, b'c', b'o', b'm', b'p', b'r', b'e', b's', b's', b'i', b'o',
        b'n', // "compression"
        0x64, b'n', b'o', b'n', b'e', // "none"
        0x64, b'd', b'a', b't', b'a', // "data"
        0x5a, 0xff, 0xff, 0xff, 0xff, // bytes(u32::MAX)
    ]);

    let start = Instant::now();
    assert!(decode_wire_frame(&bytes).is_err());
    assert!(
        start.elapsed() < Duration::from_millis(250),
        "decode took too long"
    );

    // `RpcPayload::Request(Request::IndexShard { files: <huge array> })` with a bogus array length.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&[
        0xa2, // map(2)
        0x64, b't', b'y', b'p', b'e', // "type"
        0x67, b'r', b'e', b'q', b'u', b'e', b's', b't', // "request"
        0x64, b'b', b'o', b'd', b'y', // "body"
        0xa3, // map(3)
        0x64, b't', b'y', b'p', b'e', // "type"
        0x6b, b'i', b'n', b'd', b'e', b'x', b'_', b's', b'h', b'a', b'r',
        b'd', // "index_shard"
        0x68, b'r', b'e', b'v', b'i', b's', b'i', b'o', b'n', // "revision"
        0x01, // 1
        0x65, b'f', b'i', b'l', b'e', b's', // "files"
        0x9a, 0xff, 0xff, 0xff, 0xff, // array(u32::MAX)
    ]);

    let start = Instant::now();
    assert!(decode_rpc_payload(&bytes).is_err());
    assert!(
        start.elapsed() < Duration::from_millis(250),
        "decode took too long"
    );

    // Map with `symbols: array(1_000_000)` but only 1 byte per array item.
    // Without a ratio check, `serde_cbor` would reserve space for 1M `Symbol` structs even though
    // the payload is only ~1MiB and deserialization would fail immediately on the first element.
    let mut bytes = Vec::with_capacity(1_000_000 + 32);
    bytes.extend_from_slice(&[
        0xa1, // map(1)
        0x67, b's', b'y', b'm', b'b', b'o', b'l', b's', // "symbols"
        0x9a, 0x00, 0x0f, 0x42, 0x40, // array(1_000_000)
    ]);
    bytes.resize(bytes.len() + 1_000_000, 0x00);

    let start = Instant::now();
    assert!(decode_rpc_payload(&bytes).is_err());
    assert!(
        start.elapsed() < Duration::from_millis(250),
        "decode took too long"
    );
}
