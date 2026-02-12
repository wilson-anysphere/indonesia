use nova_remote_proto::v3::{
    self, Capabilities, CompressionAlgo, HandshakeReject, ProtocolVersion, RejectCode, Request,
    Response, RpcError as ProtoRpcError, RpcErrorCode, RpcPayload, SupportedVersions, WireFrame,
    WorkerHello,
};
use nova_remote_proto::{FileText, WorkerStats};
use nova_remote_rpc::{
    RouterConfig, RpcConnection, RpcTransportError, DEFAULT_PRE_HANDSHAKE_MAX_FRAME_LEN,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn hello(auth_token: Option<String>) -> WorkerHello {
    WorkerHello {
        shard_id: 7,
        auth_token,
        supported_versions: SupportedVersions {
            min: ProtocolVersion::CURRENT,
            max: ProtocolVersion::CURRENT,
        },
        capabilities: Capabilities {
            max_frame_len: 64 * 1024,
            max_packet_len: 8 * 1024 * 1024,
            supported_compression: vec![CompressionAlgo::None],
            supports_cancel: true,
            supports_chunking: true,
        },
        cached_index_info: None,
        worker_build: None,
    }
}

fn handler_error(message: impl Into<String>) -> ProtoRpcError {
    ProtoRpcError {
        code: RpcErrorCode::InvalidRequest,
        message: message.into(),
        retryable: false,
        details: None,
    }
}

#[tokio::test]
async fn handshake_succeeds_for_matching_versions() {
    let (router_io, worker_io) = tokio::io::duplex(64 * 1024);

    let worker_hello = hello(None);

    let (router, worker) = tokio::try_join!(
        async { RpcConnection::handshake_as_router(router_io, None).await },
        async { RpcConnection::handshake_as_worker(worker_io, worker_hello).await }
    )
    .unwrap();

    let (router_conn, router_welcome) = router;
    let (worker_conn, worker_welcome) = worker;

    assert_eq!(router_welcome, worker_welcome);

    router_conn.shutdown().await.unwrap();
    worker_conn.shutdown().await.unwrap();
}

#[tokio::test]
async fn handshake_rejects_auth_mismatch() {
    let (router_io, worker_io) = tokio::io::duplex(64 * 1024);

    let worker_hello = hello(Some("wrong".into()));

    let (router_res, worker_res) = tokio::join!(
        async { RpcConnection::handshake_as_router(router_io, Some("expected")).await },
        async { RpcConnection::handshake_as_worker(worker_io, worker_hello).await }
    );

    assert!(router_res.is_err());
    assert!(worker_res.is_err());
}

#[tokio::test]
async fn handshake_rejects_unsupported_version() {
    let (router_io, worker_io) = tokio::io::duplex(64 * 1024);

    let mut worker_hello = hello(None);
    worker_hello.supported_versions = SupportedVersions {
        min: ProtocolVersion {
            major: 99,
            minor: 0,
        },
        max: ProtocolVersion {
            major: 99,
            minor: 0,
        },
    };

    let (router_res, worker_res) = tokio::join!(
        async { RpcConnection::handshake_as_router(router_io, None).await },
        async { RpcConnection::handshake_as_worker(worker_io, worker_hello).await }
    );

    assert!(router_res.is_err());
    assert!(worker_res.is_err());
}

#[tokio::test]
async fn handshake_rejects_zero_max_frame_len() {
    let (router_io, mut worker_io) = tokio::io::duplex(64 * 1024);

    let router_task =
        tokio::spawn(async move { RpcConnection::handshake_as_router(router_io, None).await });

    let mut worker_hello = hello(None);
    worker_hello.capabilities.max_frame_len = 0;

    write_wire_frame(&mut worker_io, &WireFrame::Hello(worker_hello)).await;
    let frame = read_wire_frame(&mut worker_io, DEFAULT_PRE_HANDSHAKE_MAX_FRAME_LEN).await;
    match frame {
        WireFrame::Reject(reject) => assert_eq!(reject.code, RejectCode::InvalidRequest),
        other => panic!("expected reject frame, got {other:?}"),
    }

    let router_res = router_task.await.unwrap();
    assert!(router_res.is_err());
}

#[tokio::test]
async fn handshake_rejects_zero_max_packet_len() {
    let (router_io, mut worker_io) = tokio::io::duplex(64 * 1024);

    let router_task =
        tokio::spawn(async move { RpcConnection::handshake_as_router(router_io, None).await });

    let mut worker_hello = hello(None);
    worker_hello.capabilities.max_packet_len = 0;

    write_wire_frame(&mut worker_io, &WireFrame::Hello(worker_hello)).await;
    let frame = read_wire_frame(&mut worker_io, DEFAULT_PRE_HANDSHAKE_MAX_FRAME_LEN).await;
    match frame {
        WireFrame::Reject(reject) => assert_eq!(reject.code, RejectCode::InvalidRequest),
        other => panic!("expected reject frame, got {other:?}"),
    }

    let router_res = router_task.await.unwrap();
    assert!(router_res.is_err());
}

#[tokio::test]
async fn handshake_rejects_when_no_common_compression_algorithm() {
    let (router_io, mut worker_io) = tokio::io::duplex(64 * 1024);

    let router_task =
        tokio::spawn(async move { RpcConnection::handshake_as_router(router_io, None).await });

    let mut worker_hello = hello(None);
    // A misbehaving worker might advertise only `unknown` (or omit `"none"` entirely). The router
    // should reject the handshake because there is no mutually supported compression algorithm.
    worker_hello.capabilities.supported_compression = vec![CompressionAlgo::Unknown];

    write_wire_frame(&mut worker_io, &WireFrame::Hello(worker_hello)).await;
    let frame = read_wire_frame(&mut worker_io, DEFAULT_PRE_HANDSHAKE_MAX_FRAME_LEN).await;
    match frame {
        WireFrame::Reject(reject) => {
            assert_eq!(reject.code, RejectCode::InvalidRequest);
            assert!(
                reject.message.contains("no common compression algorithm"),
                "unexpected reject message: {:?}",
                reject.message
            );
        }
        other => panic!("expected reject frame, got {other:?}"),
    }

    let router_res = router_task.await.unwrap();
    let err = match router_res {
        Ok(_) => panic!("expected handshake to fail"),
        Err(err) => err,
    };
    assert!(
        matches!(err, RpcTransportError::HandshakeFailed { .. }),
        "unexpected router error: {err:?}"
    );
}

#[tokio::test]
async fn handshake_replies_with_legacy_error_for_v2_worker_hello() {
    use nova_remote_proto::legacy_v2::RpcMessage as LegacyMessage;

    let (router_io, mut worker_io) = tokio::io::duplex(64 * 1024);

    let router_task =
        tokio::spawn(async move { RpcConnection::handshake_as_router(router_io, None).await });

    // Simulate a legacy v2 worker sending a v2 WorkerHello in a length-prefixed frame.
    let hello = LegacyMessage::WorkerHello {
        shard_id: 7,
        auth_token: None,
        has_cached_index: false,
    };
    let payload = nova_remote_proto::encode_message(&hello).unwrap();
    let len: u32 = payload.len().try_into().unwrap();
    worker_io.write_u32_le(len).await.unwrap();
    worker_io.write_all(&payload).await.unwrap();
    worker_io.flush().await.unwrap();

    // The v3 router implementation replies with a legacy v2 Error frame for clearer diagnostics.
    let len = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        worker_io.read_u32_le(),
    )
    .await
    .expect("timed out waiting for router reply")
    .unwrap();
    let mut buf = vec![0u8; len as usize];
    worker_io.read_exact(&mut buf).await.unwrap();
    let msg = nova_remote_proto::decode_message(&buf).unwrap();
    match msg {
        LegacyMessage::Error { message } => assert_eq!(message, "router only supports v3"),
        other => panic!("expected legacy error response, got {other:?}"),
    }

    let router_res = router_task.await.unwrap();
    let err = match router_res {
        Ok(_) => panic!("expected router handshake to fail for legacy v2 worker"),
        Err(err) => err,
    };
    match err {
        RpcTransportError::HandshakeFailed { message } => {
            assert_eq!(message, "router only supports v3");
        }
        other => panic!("unexpected router error: {other:?}"),
    }
}

#[tokio::test]
async fn handshake_allows_router_to_reject_before_welcome() {
    let (router_io, mut worker_io) = tokio::io::duplex(64 * 1024);

    write_wire_frame(&mut worker_io, &WireFrame::Hello(hello(None))).await;

    let expected_reject = HandshakeReject {
        code: RejectCode::InvalidRequest,
        message: "not admitted".into(),
    };
    let reject_for_hook = expected_reject.clone();

    let (router_res, frame) = tokio::join!(
        async {
            RpcConnection::handshake_as_router_with_config_and_admit(
                router_io,
                RouterConfig::default(),
                |_hello| Err(reject_for_hook),
            )
            .await
        },
        async { read_wire_frame(&mut worker_io, DEFAULT_PRE_HANDSHAKE_MAX_FRAME_LEN).await },
    );

    let err = match router_res {
        Ok(_) => panic!("expected router to reject in admission hook"),
        Err(err) => err,
    };
    assert!(matches!(err, RpcTransportError::HandshakeFailed { .. }));

    match frame {
        WireFrame::Reject(reject) => assert_eq!(reject, expected_reject),
        other => panic!("expected reject frame, got {other:?}"),
    }
}

#[tokio::test]
async fn handshake_reject_messages_are_sanitized() {
    let (router_io, mut worker_io) = tokio::io::duplex(64 * 1024);

    let secret_suffix = "nova-remote-rpc-handshake-reject-secret";
    let raw_message =
        format!("invalid type: string \"{secret_suffix}\", expected boolean at line 1 column 1");
    assert!(
        raw_message.contains(secret_suffix),
        "expected raw reject message to include secret so this test catches leaks: {raw_message}"
    );

    let reject_for_hook = HandshakeReject {
        code: RejectCode::InvalidRequest,
        message: raw_message,
    };

    let router_task = tokio::spawn(async move {
        RpcConnection::handshake_as_router_with_config_and_admit(
            router_io,
            RouterConfig::default(),
            |_hello| Err(reject_for_hook),
        )
        .await
    });

    // Manual worker handshake so we can inspect the reject frame itself (not just the locally
    // formatted error message).
    write_wire_frame(
        &mut worker_io,
        &WireFrame::Hello(hello(None)),
    )
    .await;
    let frame = read_wire_frame(&mut worker_io, DEFAULT_PRE_HANDSHAKE_MAX_FRAME_LEN).await;
    match frame {
        WireFrame::Reject(reject) => {
            assert!(
                !reject.message.contains(secret_suffix),
                "expected reject frame message to be sanitized: {:?}",
                reject.message
            );
            assert!(
                reject.message.contains("<redacted>"),
                "expected reject frame message to include redaction marker: {:?}",
                reject.message
            );
        }
        other => panic!("expected reject frame, got {other:?}"),
    }

    let router_res = router_task.await.unwrap();
    let err = match router_res {
        Ok(_) => panic!("expected router handshake to fail"),
        Err(err) => err,
    };
    match err {
        RpcTransportError::HandshakeFailed { message } => {
            assert!(
                !message.contains(secret_suffix),
                "expected router handshake error message to be sanitized: {message}"
            );
            assert!(
                message.contains("<redacted>"),
                "expected router handshake error message to include redaction marker: {message}"
            );
        }
        other => panic!("unexpected router error: {other:?}"),
    }
}

#[tokio::test]
async fn multiplexing_matches_responses_by_id() {
    let (router_io, worker_io) = tokio::io::duplex(64 * 1024);

    let (router, worker) = tokio::try_join!(
        async { RpcConnection::handshake_as_router(router_io, None).await },
        async { RpcConnection::handshake_as_worker(worker_io, hello(None)).await }
    )
    .unwrap();

    let (router, _) = router;
    let (worker, _) = worker;

    worker.set_request_handler(|_ctx, req| async move {
        match req {
            Request::UpdateFile { revision, .. } => {
                // Force an out-of-order response with staggered delays.
                let delay_ms = 20u64.saturating_sub(revision.min(20));
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                Ok(Response::WorkerStats(WorkerStats {
                    shard_id: 7,
                    revision,
                    index_generation: 0,
                    file_count: revision as u32,
                }))
            }
            other => Err(handler_error(format!("unexpected request: {other:?}"))),
        }
    });

    let mut tasks = Vec::new();
    for i in 1u64..=20 {
        let conn = router.clone();
        tasks.push(tokio::spawn(async move {
            conn.call(Request::UpdateFile {
                revision: i,
                file: FileText {
                    path: format!("file{i}.java"),
                    text: "class A {}".into(),
                },
            })
            .await
            .unwrap()
        }));
    }

    let mut got = Vec::new();
    for task in tasks {
        got.push(task.await.unwrap());
    }

    let mut counts: Vec<u32> = got
        .into_iter()
        .map(|resp| match resp {
            Response::WorkerStats(ws) => ws.file_count,
            other => panic!("unexpected response: {other:?}"),
        })
        .collect();

    counts.sort();
    assert_eq!(counts, (1u32..=20).collect::<Vec<_>>());

    router.shutdown().await.unwrap();
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn closed_signal_fires_when_peer_drops_stream() {
    let (router_io, mut worker_io) = tokio::io::duplex(64 * 1024);

    let router_task = tokio::spawn(async move {
        RpcConnection::handshake_as_router(router_io, None)
            .await
            .unwrap()
            .0
    });

    // Manual worker handshake so we can drop the stream without starting a worker RpcConnection.
    write_wire_frame(&mut worker_io, &WireFrame::Hello(hello(None))).await;
    let frame = read_wire_frame(&mut worker_io, DEFAULT_PRE_HANDSHAKE_MAX_FRAME_LEN).await;
    assert!(matches!(frame, WireFrame::Welcome(_)));

    let router = router_task.await.unwrap();

    let mut closed_rx = router.subscribe_closed();
    assert!(!*closed_rx.borrow(), "connection should start open");

    drop(worker_io);

    tokio::time::timeout(std::time::Duration::from_millis(200), closed_rx.changed())
        .await
        .expect("timed out waiting for closed signal")
        .unwrap();
    assert!(*closed_rx.borrow(), "closed signal should become true");

    let err = router.wait_closed().await;
    assert!(
        matches!(
            err,
            RpcTransportError::Io { .. } | RpcTransportError::ConnectionClosed
        ),
        "unexpected close error: {err:?}"
    );
}

#[tokio::test]
async fn chunking_reassembles_large_packets() {
    let (router_io, worker_io) = tokio::io::duplex(64 * 1024);

    let mut worker_hello = hello(None);
    worker_hello.capabilities.max_frame_len = 8 * 1024;
    worker_hello.capabilities.max_packet_len = 4 * 1024 * 1024;

    let (router, worker) = tokio::try_join!(
        async { RpcConnection::handshake_as_router(router_io, None).await },
        async { RpcConnection::handshake_as_worker(worker_io, worker_hello).await }
    )
    .unwrap();

    let (router, _) = router;
    let (worker, _) = worker;

    worker.set_request_handler(|_ctx, req| async move {
        match req {
            Request::LoadFiles { files, .. } => {
                assert_eq!(files.len(), 1);
                assert_eq!(files[0].text.len(), 1024 * 1024);
                Ok(Response::Ack)
            }
            other => Err(handler_error(format!("unexpected request: {other:?}"))),
        }
    });

    let large_text = "a".repeat(1024 * 1024);
    let resp = router
        .call(Request::LoadFiles {
            revision: 1,
            files: vec![FileText {
                path: "a.java".into(),
                text: large_text,
            }],
        })
        .await
        .unwrap();

    assert!(matches!(resp, Response::Ack));

    router.shutdown().await.unwrap();
    worker.shutdown().await.unwrap();
}

#[cfg(feature = "zstd")]
#[tokio::test]
async fn compression_roundtrips() {
    let (router_io, worker_io) = tokio::io::duplex(64 * 1024);

    let mut worker_hello = hello(None);
    worker_hello.capabilities.supported_compression =
        vec![CompressionAlgo::Zstd, CompressionAlgo::None];

    let (router, worker) = tokio::try_join!(
        async { RpcConnection::handshake_as_router(router_io, None).await },
        async { RpcConnection::handshake_as_worker(worker_io, worker_hello).await }
    )
    .unwrap();

    let (router, welcome) = router;
    let (worker, _) = worker;

    assert!(welcome
        .chosen_capabilities
        .supported_compression
        .contains(&CompressionAlgo::Zstd));

    worker.set_request_handler(|_ctx, req| async move {
        match req {
            Request::LoadFiles { .. } => Ok(Response::Ack),
            other => Err(handler_error(format!("unexpected request: {other:?}"))),
        }
    });

    let large_text = "x".repeat(1024 * 1024);
    let resp = router
        .call(Request::LoadFiles {
            revision: 1,
            files: vec![FileText {
                path: "a.java".into(),
                text: large_text,
            }],
        })
        .await
        .unwrap();
    assert!(matches!(resp, Response::Ack));

    router.shutdown().await.unwrap();
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn request_id_parity_router_even_worker_odd() {
    let (router_io, worker_io) = tokio::io::duplex(64 * 1024);

    let (router, worker) = tokio::try_join!(
        async { RpcConnection::handshake_as_router(router_io, None).await },
        async { RpcConnection::handshake_as_worker(worker_io, hello(None)).await }
    )
    .unwrap();

    let (router, _) = router;
    let (worker, _) = worker;

    worker.set_request_handler(|ctx, _req| async move {
        assert_eq!(
            ctx.request_id() % 2,
            0,
            "router must generate even request ids"
        );
        Ok(Response::Ack)
    });

    router.set_request_handler(|ctx, _req| async move {
        assert_eq!(
            ctx.request_id() % 2,
            1,
            "worker must generate odd request ids"
        );
        Ok(Response::Ack)
    });

    let resp = router.call(Request::GetWorkerStats).await.unwrap();
    assert!(matches!(resp, Response::Ack));

    let resp = worker.call(Request::GetWorkerStats).await.unwrap();
    assert!(matches!(resp, Response::Ack));

    router.shutdown().await.unwrap();
    worker.shutdown().await.unwrap();
}

async fn write_wire_frame(stream: &mut (impl tokio::io::AsyncWrite + Unpin), frame: &WireFrame) {
    let payload = v3::encode_wire_frame(frame).unwrap();
    let len: u32 = payload.len().try_into().unwrap();
    stream.write_u32_le(len).await.unwrap();
    stream.write_all(&payload).await.unwrap();
    stream.flush().await.unwrap();
}

async fn read_wire_frame(
    stream: &mut (impl tokio::io::AsyncRead + Unpin),
    max_frame_len: u32,
) -> WireFrame {
    let len = stream.read_u32_le().await.unwrap();
    assert!(
        len <= max_frame_len,
        "frame too large: {len} > {max_frame_len}"
    );
    let mut buf = vec![0u8; len as usize];
    stream.read_exact(&mut buf).await.unwrap();
    v3::decode_wire_frame(&buf).unwrap()
}

#[tokio::test]
async fn inbound_parity_violation_closes_connection() {
    let (router_io, mut worker_io) = tokio::io::duplex(64 * 1024);

    let router_task = tokio::spawn(async move {
        RpcConnection::handshake_as_router(router_io, None)
            .await
            .unwrap()
            .0
    });

    // Manual worker handshake.
    write_wire_frame(&mut worker_io, &WireFrame::Hello(hello(None))).await;
    let frame = read_wire_frame(&mut worker_io, DEFAULT_PRE_HANDSHAKE_MAX_FRAME_LEN).await;
    assert!(matches!(frame, WireFrame::Welcome(_)));

    let router = router_task.await.unwrap();

    // Worker-initiated requests must be odd; send an even id.
    let payload = v3::encode_rpc_payload(&RpcPayload::Request(Request::GetWorkerStats)).unwrap();
    write_wire_frame(
        &mut worker_io,
        &WireFrame::Packet {
            id: 2,
            compression: CompressionAlgo::None,
            data: payload,
        },
    )
    .await;

    // Router should close the connection.
    let err = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        worker_io.read_u32_le(),
    )
    .await
    .expect("timed out waiting for router to close")
    .expect_err("expected EOF after protocol violation");
    assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);

    let err = router
        .notify(nova_remote_proto::v3::Notification::Unknown)
        .await
        .expect_err("expected router to be closed");
    assert!(matches!(err, RpcTransportError::ProtocolViolation { .. }));
}

#[tokio::test]
async fn reserved_request_id_zero_closes_connection() {
    let (router_io, mut worker_io) = tokio::io::duplex(64 * 1024);

    let router_task = tokio::spawn(async move {
        RpcConnection::handshake_as_router(router_io, None)
            .await
            .unwrap()
            .0
    });

    // Manual worker handshake.
    write_wire_frame(&mut worker_io, &WireFrame::Hello(hello(None))).await;
    let frame = read_wire_frame(&mut worker_io, DEFAULT_PRE_HANDSHAKE_MAX_FRAME_LEN).await;
    assert!(matches!(frame, WireFrame::Welcome(_)));

    let router = router_task.await.unwrap();

    write_wire_frame(
        &mut worker_io,
        &WireFrame::Packet {
            id: 0,
            compression: CompressionAlgo::None,
            data: Vec::new(),
        },
    )
    .await;

    let err = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        worker_io.read_u32_le(),
    )
    .await
    .expect("timed out waiting for router to close")
    .expect_err("expected EOF after protocol violation");
    assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);

    let err = router
        .notify(nova_remote_proto::v3::Notification::Unknown)
        .await
        .expect_err("expected router to be closed");
    assert!(matches!(err, RpcTransportError::ProtocolViolation { .. }));
}

#[tokio::test]
async fn too_many_inflight_chunked_packets_closes_connection() {
    let (router_io, mut worker_io) = tokio::io::duplex(64 * 1024);

    let router_task = tokio::spawn(async move {
        RpcConnection::handshake_as_router(router_io, None)
            .await
            .unwrap()
            .0
    });

    // Manual worker handshake.
    write_wire_frame(&mut worker_io, &WireFrame::Hello(hello(None))).await;
    let frame = read_wire_frame(&mut worker_io, DEFAULT_PRE_HANDSHAKE_MAX_FRAME_LEN).await;
    assert!(matches!(frame, WireFrame::Welcome(_)));

    let router = router_task.await.unwrap();

    // `nova-remote-rpc` enforces a cap on the number of concurrently in-flight chunk reassemblies
    // to prevent a peer from exhausting memory by interleaving many `PacketChunk` streams.
    //
    // Keep this value in sync with `MAX_INFLIGHT_CHUNKED_PACKETS` in `nova-remote-rpc`.
    const MAX_INFLIGHT: usize = 32;
    for i in 0..=MAX_INFLIGHT {
        let id = 1 + (i as u64) * 2;
        write_wire_frame(
            &mut worker_io,
            &WireFrame::PacketChunk {
                id,
                compression: CompressionAlgo::None,
                seq: 0,
                last: false,
                data: vec![0u8],
            },
        )
        .await;
    }

    // Router should close the connection.
    let err = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        worker_io.read_u32_le(),
    )
    .await
    .expect("timed out waiting for router to close")
    .expect_err("expected EOF after protocol violation");
    assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);

    let err = router
        .notify(nova_remote_proto::v3::Notification::Unknown)
        .await
        .expect_err("expected router to be closed");
    match err {
        RpcTransportError::ProtocolViolation { message } => {
            assert!(message.contains("too many in-flight chunked packets"));
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test]
async fn cancel_sent_immediately_after_request_is_observed() {
    use std::sync::{Arc, Mutex};

    let (router_io, mut worker_io) = tokio::io::duplex(64 * 1024);

    let router_task = tokio::spawn(async move {
        RpcConnection::handshake_as_router(router_io, None)
            .await
            .unwrap()
            .0
    });

    write_wire_frame(&mut worker_io, &WireFrame::Hello(hello(None))).await;
    let frame = read_wire_frame(&mut worker_io, DEFAULT_PRE_HANDSHAKE_MAX_FRAME_LEN).await;
    assert!(matches!(frame, WireFrame::Welcome(_)));

    let router = router_task.await.unwrap();

    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
    let done_tx = Arc::new(Mutex::new(Some(done_tx)));

    router.set_request_handler(move |ctx, _req| {
        let done_tx = done_tx.clone();
        async move {
            let mut token = ctx.cancellation();
            if !token.is_cancelled() {
                token.cancelled().await;
            }

            if let Some(done_tx) = done_tx.lock().unwrap().take() {
                let _ = done_tx.send(());
            }

            Ok(Response::Ack)
        }
    });

    let request = v3::encode_rpc_payload(&RpcPayload::Request(Request::GetWorkerStats)).unwrap();
    let cancel = v3::encode_rpc_payload(&RpcPayload::Cancel).unwrap();

    // Send Request and immediately send Cancel on the same request_id. Historically this could race
    // such that Cancel arrived before the cancellation token was registered.
    write_wire_frame(
        &mut worker_io,
        &WireFrame::Packet {
            id: 1,
            compression: CompressionAlgo::None,
            data: request,
        },
    )
    .await;
    write_wire_frame(
        &mut worker_io,
        &WireFrame::Packet {
            id: 1,
            compression: CompressionAlgo::None,
            data: cancel,
        },
    )
    .await;

    tokio::time::timeout(std::time::Duration::from_millis(200), done_rx)
        .await
        .expect("timed out waiting for handler to observe cancellation")
        .unwrap();

    router.shutdown().await.unwrap();
}

#[tokio::test]
async fn chunking_reassembles_interleaved_packets() {
    use std::collections::HashMap;

    let (router_io, mut worker_io) = tokio::io::duplex(1024 * 1024);

    // Use small frames so the request payloads must be chunked.
    let mut worker_hello = hello(None);
    worker_hello.capabilities.max_frame_len = 4096;
    worker_hello.capabilities.max_packet_len = 2 * 1024 * 1024;
    worker_hello.capabilities.supported_compression = vec![CompressionAlgo::None];
    worker_hello.capabilities.supports_cancel = false;
    worker_hello.capabilities.supports_chunking = true;

    let router_task = tokio::spawn(async move {
        RpcConnection::handshake_as_router(router_io, None)
            .await
            .unwrap()
            .0
    });

    // Manual worker handshake.
    write_wire_frame(&mut worker_io, &WireFrame::Hello(worker_hello)).await;
    let frame = read_wire_frame(&mut worker_io, DEFAULT_PRE_HANDSHAKE_MAX_FRAME_LEN).await;
    let welcome = match frame {
        WireFrame::Welcome(welcome) => welcome,
        other => panic!("expected welcome frame, got {other:?}"),
    };
    assert!(welcome.chosen_capabilities.supports_chunking);
    assert_eq!(welcome.chosen_capabilities.max_frame_len, 4096);

    let router = router_task.await.unwrap();

    router.set_request_handler(|ctx, req| async move {
        // For this test we only care that both interleaved packets get reassembled and dispatched.
        assert!(matches!(req, Request::LoadFiles { .. }));
        // Inbound requests are initiated by the worker, so IDs must be odd.
        assert_eq!(ctx.request_id() % 2, 1);
        Ok(Response::Ack)
    });

    let payload_a = "a".repeat(120_000);
    let payload_b = "b".repeat(100_000);

    let req_a = Request::LoadFiles {
        revision: 1,
        files: vec![FileText {
            path: "a.java".into(),
            text: payload_a,
        }],
    };
    let req_b = Request::LoadFiles {
        revision: 2,
        files: vec![FileText {
            path: "b.java".into(),
            text: payload_b,
        }],
    };

    let bytes_a = v3::encode_rpc_payload(&RpcPayload::Request(req_a)).unwrap();
    let bytes_b = v3::encode_rpc_payload(&RpcPayload::Request(req_b)).unwrap();

    // Chunk size chosen to comfortably fit into `max_frame_len` once CBOR overhead is included.
    let chunk_size = 1024usize;
    let chunks_a: Vec<Vec<u8>> = bytes_a.chunks(chunk_size).map(|c| c.to_vec()).collect();
    let chunks_b: Vec<Vec<u8>> = bytes_b.chunks(chunk_size).map(|c| c.to_vec()).collect();

    // Worker request IDs are odd; use two distinct IDs and interleave the chunk streams.
    let id_a: u64 = 1;
    let id_b: u64 = 3;

    let max_chunks = chunks_a.len().max(chunks_b.len());
    for seq in 0..max_chunks {
        if let Some(chunk) = chunks_a.get(seq) {
            write_wire_frame(
                &mut worker_io,
                &WireFrame::PacketChunk {
                    id: id_a,
                    compression: CompressionAlgo::None,
                    seq: seq as u32,
                    last: seq + 1 == chunks_a.len(),
                    data: chunk.clone(),
                },
            )
            .await;
        }
        if let Some(chunk) = chunks_b.get(seq) {
            write_wire_frame(
                &mut worker_io,
                &WireFrame::PacketChunk {
                    id: id_b,
                    compression: CompressionAlgo::None,
                    seq: seq as u32,
                    last: seq + 1 == chunks_b.len(),
                    data: chunk.clone(),
                },
            )
            .await;
        }
    }

    // Expect two Ack responses, one per request id, in any order.
    let mut seen: HashMap<u64, Response> = HashMap::new();
    while seen.len() < 2 {
        let frame =
            read_wire_frame(&mut worker_io, welcome.chosen_capabilities.max_frame_len).await;
        match frame {
            WireFrame::Packet {
                id,
                compression,
                data,
            } => {
                assert_eq!(compression, CompressionAlgo::None);
                let payload = v3::decode_rpc_payload(&data).unwrap();
                match payload {
                    RpcPayload::Response(result) => match result {
                        nova_remote_proto::v3::RpcResult::Ok { value } => {
                            seen.insert(id, value);
                        }
                        other => panic!("unexpected rpc result: {other:?}"),
                    },
                    other => panic!("unexpected payload: {other:?}"),
                }
            }
            other => panic!("unexpected frame: {other:?}"),
        }
    }

    assert!(matches!(seen.get(&id_a), Some(Response::Ack)));
    assert!(matches!(seen.get(&id_b), Some(Response::Ack)));

    router.shutdown().await.unwrap();
}
