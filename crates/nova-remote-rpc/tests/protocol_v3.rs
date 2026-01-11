use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use anyhow::{anyhow, Context as _, Result};
use nova_remote_proto::v3::{
    self, Capabilities, CompressionAlgo, ProtocolVersion, RejectCode, Request, Response,
    RpcPayload, RpcResult, SupportedVersions, WireFrame,
};
use nova_remote_proto::{FileText, WorkerStats};
use nova_remote_rpc::{
    Client, ClientConfig, Server, ServerConfig, DEFAULT_PRE_HANDSHAKE_MAX_FRAME_LEN,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};

#[tokio::test(flavor = "current_thread")]
async fn multiplexing_correlates_out_of_order_responses() -> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);

        let server_task = tokio::spawn(async move {
            let mut server = Server::accept(server_stream, ServerConfig::default()).await?;

            let mut delayed = Vec::new();
            for _ in 0..20u64 {
                let req = server
                    .recv_request()
                    .await
                    .ok_or_else(|| anyhow!("missing request"))?;
                match req.request {
                    Request::LoadFiles { revision, .. } => {
                        if revision % 2 == 0 {
                            delayed.push((req.request_id, revision));
                        } else {
                            server
                                .respond_ok(
                                    req.request_id,
                                    Response::WorkerStats(WorkerStats {
                                        shard_id: 0,
                                        revision,
                                        index_generation: 0,
                                        file_count: revision as u32,
                                    }),
                                )
                                .await?;
                        }
                    }
                    other => return Err(anyhow!("unexpected request: {other:?}")),
                }
            }

            // Respond to the delayed requests last to guarantee out-of-order delivery.
            for (request_id, revision) in delayed {
                server
                    .respond_ok(
                        request_id,
                        Response::WorkerStats(WorkerStats {
                            shard_id: 0,
                            revision,
                            index_generation: 0,
                            file_count: revision as u32,
                        }),
                    )
                    .await?;
            }

            Ok::<_, anyhow::Error>(())
        });

        let client = Arc::new(Client::connect(client_stream, ClientConfig::default()).await?);

        let mut set = tokio::task::JoinSet::new();
        for revision in 0..20u64 {
            let client = client.clone();
            set.spawn(async move {
                let resp = client
                    .call(Request::LoadFiles {
                        revision,
                        files: Vec::new(),
                    })
                    .await?;
                match resp {
                    RpcResult::Ok {
                        value: Response::WorkerStats(stats),
                    } => {
                        assert_eq!(stats.revision, revision);
                        Ok::<_, anyhow::Error>(())
                    }
                    other => Err(anyhow!("unexpected response: {other:?}")),
                }
            });
        }

        while let Some(res) = set.join_next().await {
            res??;
        }

        server_task.await??;
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn chunking_reassembles_large_payload() -> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        let (client_stream, server_stream) = tokio::io::duplex(1024 * 1024);

        let mut client_cfg = ClientConfig::default();
        client_cfg.hello.capabilities = Capabilities {
            max_frame_len: 4096,
            max_packet_len: 2 * 1024 * 1024,
            supported_compression: vec![CompressionAlgo::None],
            supports_cancel: false,
            supports_chunking: true,
        };
        client_cfg.compression_threshold = usize::MAX;

        let mut server_cfg = ServerConfig::default();
        server_cfg.capabilities = Capabilities {
            max_frame_len: 4096,
            max_packet_len: 2 * 1024 * 1024,
            supported_compression: vec![CompressionAlgo::None],
            supports_cancel: false,
            supports_chunking: true,
        };
        server_cfg.compression_threshold = usize::MAX;

        let payload = "a".repeat(200_000);
        let request = Request::IndexShard {
            revision: 1,
            files: vec![FileText {
                path: "big.java".into(),
                text: payload.clone(),
            }],
        };

        let server_task = tokio::spawn(async move {
            let mut server = Server::accept(server_stream, server_cfg).await?;
            let req = server
                .recv_request()
                .await
                .ok_or_else(|| anyhow!("missing request"))?;
            match req.request {
                Request::IndexShard { revision, files } => {
                    assert_eq!(revision, 1);
                    assert_eq!(files.len(), 1);
                    assert_eq!(files[0].text, payload);
                }
                other => return Err(anyhow!("unexpected request: {other:?}")),
            }
            server.respond_ok(req.request_id, Response::Ack).await?;
            Ok::<_, anyhow::Error>(())
        });

        let client = Client::connect(client_stream, client_cfg).await?;
        let resp = client.call(request).await?;
        match resp {
            RpcResult::Ok { value } => assert!(matches!(value, Response::Ack)),
            other => return Err(anyhow!("unexpected response: {other:?}")),
        }

        server_task.await??;
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn compression_zstd_roundtrips_and_is_smaller_on_wire() -> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        let (client_stream, server_stream) = tokio::io::duplex(1024 * 1024);
        let bytes_written = Arc::new(AtomicUsize::new(0));
        let client_stream = CountingStream::new(client_stream, bytes_written.clone());

        let mut client_cfg = ClientConfig::default();
        client_cfg.compression_threshold = 1;

        let mut server_cfg = ServerConfig::default();
        server_cfg.compression_threshold = 1;

        // Large and extremely compressible.
        let payload = "aaaaaaaaaaaaaaaa".repeat(25_000);
        let request = Request::LoadFiles {
            revision: 1,
            files: vec![FileText {
                path: "big.java".into(),
                text: payload,
            }],
        };

        let uncompressed = v3::encode_rpc_payload(&RpcPayload::Request(request.clone()))
            .context("encode uncompressed payload")?;
        let uncompressed_len = uncompressed.len();

        let server_task = tokio::spawn(async move {
            let mut server = Server::accept(server_stream, server_cfg).await?;
            let req = server
                .recv_request()
                .await
                .ok_or_else(|| anyhow!("missing request"))?;
            assert!(matches!(req.request, Request::LoadFiles { .. }));
            server.respond_ok(req.request_id, Response::Ack).await?;
            Ok::<_, anyhow::Error>(())
        });

        let client = Client::connect(client_stream, client_cfg).await?;
        let baseline = bytes_written.load(Ordering::Relaxed);
        let resp = client.call(request).await?;
        match resp {
            RpcResult::Ok { value } => assert!(matches!(value, Response::Ack)),
            other => return Err(anyhow!("unexpected response: {other:?}")),
        }

        let delta = bytes_written.load(Ordering::Relaxed) - baseline;
        assert!(
            delta < uncompressed_len,
            "expected compressed on-wire bytes ({delta}) < uncompressed payload bytes ({uncompressed_len})"
        );

        server_task.await??;
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn handshake_rejects_missing_auth_token() -> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        let (mut client_stream, server_stream) = tokio::io::duplex(64 * 1024);

        let mut server_cfg = ServerConfig::default();
        server_cfg.expected_auth_token = Some("secret".to_string());

        let server_task =
            tokio::spawn(async move { Server::accept(server_stream, server_cfg).await });

        let mut client_cfg = ClientConfig::default();
        client_cfg.hello.auth_token = None;

        write_frame(&mut client_stream, &WireFrame::Hello(client_cfg.hello)).await?;
        let frame = read_frame(&mut client_stream, DEFAULT_PRE_HANDSHAKE_MAX_FRAME_LEN).await?;
        match frame {
            WireFrame::Reject(reject) => assert_eq!(reject.code, RejectCode::Unauthorized),
            other => return Err(anyhow!("expected Reject, got {other:?}")),
        }

        let _ = server_task.await;
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn handshake_rejects_unsupported_version() -> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        let (mut client_stream, server_stream) = tokio::io::duplex(64 * 1024);

        let server_task =
            tokio::spawn(
                async move { Server::accept(server_stream, ServerConfig::default()).await },
            );

        let mut client_cfg = ClientConfig::default();
        let unsupported = ProtocolVersion { major: 2, minor: 0 };
        client_cfg.hello.supported_versions = SupportedVersions {
            min: unsupported,
            max: unsupported,
        };

        write_frame(&mut client_stream, &WireFrame::Hello(client_cfg.hello)).await?;
        let frame = read_frame(&mut client_stream, DEFAULT_PRE_HANDSHAKE_MAX_FRAME_LEN).await?;
        match frame {
            WireFrame::Reject(reject) => assert_eq!(reject.code, RejectCode::UnsupportedVersion),
            other => return Err(anyhow!("expected Reject, got {other:?}")),
        }

        let _ = server_task.await;
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn max_packet_len_closes_connection_deterministically() -> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        let (mut client_stream, server_stream) = tokio::io::duplex(64 * 1024);

        let mut server_cfg = ServerConfig::default();
        server_cfg.capabilities.max_frame_len = 64 * 1024;
        server_cfg.capabilities.max_packet_len = 1024;
        server_cfg.capabilities.supported_compression = vec![CompressionAlgo::None];
        server_cfg.capabilities.supports_chunking = false;

        let server_task =
            tokio::spawn(async move { Server::accept(server_stream, server_cfg).await });

        // Manual handshake.
        let mut client_cfg = ClientConfig::default();
        client_cfg.hello.capabilities.max_frame_len = 64 * 1024;
        client_cfg.hello.capabilities.max_packet_len = 1024;
        client_cfg.hello.capabilities.supported_compression = vec![CompressionAlgo::None];
        client_cfg.hello.capabilities.supports_chunking = false;

        write_frame(&mut client_stream, &WireFrame::Hello(client_cfg.hello)).await?;
        let frame = read_frame(&mut client_stream, DEFAULT_PRE_HANDSHAKE_MAX_FRAME_LEN).await?;
        let WireFrame::Welcome(_) = frame else {
            return Err(anyhow!("expected Welcome, got {frame:?}"));
        };

        let mut server = server_task.await??;

        // Send a deliberately invalid packet whose on-wire data exceeds max_packet_len.
        let too_large = WireFrame::Packet {
            id: 1,
            compression: CompressionAlgo::None,
            data: vec![0u8; 1025],
        };
        write_frame(&mut client_stream, &too_large).await?;

        // The receiver should close rather than attempting to allocate unbounded buffers.
        let req =
            tokio::time::timeout(std::time::Duration::from_millis(100), server.recv_request())
                .await
                .context("recv_request timed out")?;
        assert!(req.is_none(), "expected receiver to close on TooLarge");

        Ok::<_, anyhow::Error>(())
    })
    .await??;
    Ok(())
}

struct CountingStream<S> {
    inner: S,
    bytes_written: Arc<AtomicUsize>,
}

impl<S> CountingStream<S> {
    fn new(inner: S, bytes_written: Arc<AtomicUsize>) -> Self {
        Self {
            inner,
            bytes_written,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for CountingStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for CountingStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let poll = Pin::new(&mut self.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(written)) = &poll {
            self.bytes_written.fetch_add(*written, Ordering::Relaxed);
        }
        poll
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

async fn write_frame(stream: &mut (impl AsyncWrite + Unpin), frame: &WireFrame) -> Result<()> {
    let payload = v3::encode_wire_frame(frame).context("encode wire frame")?;
    let len: u32 = payload.len().try_into().context("frame len overflow")?;
    stream.write_u32_le(len).await.context("write frame len")?;
    stream
        .write_all(&payload)
        .await
        .context("write frame payload")?;
    stream.flush().await.context("flush frame")?;
    Ok(())
}

async fn read_frame(
    stream: &mut (impl AsyncRead + Unpin),
    max_frame_len: u32,
) -> Result<WireFrame> {
    let len = stream.read_u32_le().await.context("read frame len")?;
    anyhow::ensure!(
        len <= max_frame_len,
        "frame too large: {len} > {max_frame_len}"
    );
    let mut buf = vec![0u8; len as usize];
    stream
        .read_exact(&mut buf)
        .await
        .context("read frame payload")?;
    v3::decode_wire_frame(&buf).context("decode wire frame")
}
