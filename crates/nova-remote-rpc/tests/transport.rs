use nova_remote_proto::v3::{
    Capabilities, CompressionAlgo, ProtocolVersion, Request, Response, RpcError as ProtoRpcError,
    RpcErrorCode, SupportedVersions, WorkerHello,
};
use nova_remote_proto::{FileText, WorkerStats};
use nova_remote_rpc::RpcConnection;

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
        min: ProtocolVersion { major: 99, minor: 0 },
        max: ProtocolVersion { major: 99, minor: 0 },
    };

    let (router_res, worker_res) = tokio::join!(
        async { RpcConnection::handshake_as_router(router_io, None).await },
        async { RpcConnection::handshake_as_worker(worker_io, worker_hello).await }
    );

    assert!(router_res.is_err());
    assert!(worker_res.is_err());
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
                let delay_ms = (20u64.saturating_sub(revision.min(20))) as u64;
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
        assert_eq!(ctx.request_id() % 2, 0, "router must generate even request ids");
        Ok(Response::Ack)
    });

    router.set_request_handler(|ctx, _req| async move {
        assert_eq!(ctx.request_id() % 2, 1, "worker must generate odd request ids");
        Ok(Response::Ack)
    });

    let resp = router.call(Request::GetWorkerStats).await.unwrap();
    assert!(matches!(resp, Response::Ack));

    let resp = worker.call(Request::GetWorkerStats).await.unwrap();
    assert!(matches!(resp, Response::Ack));

    router.shutdown().await.unwrap();
    worker.shutdown().await.unwrap();
}
