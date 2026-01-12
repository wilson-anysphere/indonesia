use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use crate::remote_rpc_util;
use nova_remote_proto::v3::{Notification, Request, Response};
use nova_remote_proto::ShardIndex;
use nova_remote_proto::{RpcMessage, ShardId, ShardIndexInfo, WorkerStats};
use nova_router::{DistributedRouterConfig, ListenAddr, QueryRouter, SourceRoot, WorkspaceLayout};
use tempfile::TempDir;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::oneshot;

#[cfg(unix)]
use tokio::net::UnixStream;

async fn write_rpc(stream: &mut (impl AsyncWrite + Unpin), message: &RpcMessage) -> Result<()> {
    let payload = nova_remote_proto::encode_message(message)?;
    let len: u32 = payload
        .len()
        .try_into()
        .map_err(|_| anyhow!("message too large"))?;
    stream.write_u32_le(len).await.context("write len")?;
    stream.write_all(&payload).await.context("write payload")?;
    stream.flush().await.context("flush")?;
    Ok(())
}

async fn read_rpc(stream: &mut (impl AsyncRead + Unpin)) -> Result<RpcMessage> {
    let len = stream.read_u32_le().await.context("read len")?;
    let mut buf = vec![0u8; len as usize];
    stream.read_exact(&mut buf).await.context("read payload")?;
    nova_remote_proto::decode_message(&buf)
}

#[cfg(unix)]
async fn connect_unix_with_retry(socket_path: &Path) -> Result<UnixStream> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        match UnixStream::connect(socket_path).await {
            Ok(stream) => return Ok(stream),
            Err(err) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(anyhow!(
                        "timed out connecting to router at {socket_path:?}: {err}"
                    ));
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
}

#[cfg(unix)]
async fn expect_disconnect(mut stream: UnixStream) -> Result<()> {
    match tokio::time::timeout(Duration::from_secs(1), read_rpc(&mut stream)).await {
        Ok(Ok(RpcMessage::Shutdown)) => Ok(()),
        Ok(Ok(other)) => Err(anyhow!(
            "expected Shutdown after protocol violation, got {other:?}"
        )),
        Ok(Err(_)) => Ok(()),
        Err(_) => Err(anyhow!("timed out waiting for router disconnect")),
    }
}

#[cfg(unix)]
async fn connect_worker(
    socket_path: &Path,
    shard_id: ShardId,
) -> Result<remote_rpc_util::ConnectedWorker<UnixStream>> {
    remote_rpc_util::connect_and_handshake_worker(
        || async { connect_unix_with_retry(socket_path).await },
        shard_id,
        None,
    )
    .await
}

#[cfg(unix)]
async fn expect_disconnect_v3(
    conn: nova_remote_rpc::RpcConnection,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
    timeout: Duration,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if *shutdown_rx.borrow() {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(anyhow!("timed out waiting for router disconnect"));
        }
        tokio::select! {
            _ = shutdown_rx.changed() => {}
            res = conn.notify(Notification::Unknown) => {
                if res.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
    let _ = conn.shutdown().await;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn update_file_rejects_cross_shard_index_poisoning() -> Result<()> {
    let tmp = TempDir::new()?;
    let workspace_root = tmp.path();

    let shard0 = workspace_root.join("shard0").join("src");
    let shard1 = workspace_root.join("shard1").join("src");
    tokio::fs::create_dir_all(&shard0).await?;
    tokio::fs::create_dir_all(&shard1).await?;

    let file0 = shard0.join("A.java");
    tokio::fs::write(&file0, "package a; public class Alpha {}").await?;
    tokio::fs::write(shard1.join("B.java"), "package b; public class Beta {}").await?;

    let listen_path = workspace_root.join("router.sock");
    let cache_dir = workspace_root.join("cache");

    let config = DistributedRouterConfig {
        listen_addr: ListenAddr::Unix(listen_path.clone()),
        worker_command: PathBuf::from("unused-worker"),
        cache_dir,
        auth_token: None,
        allow_insecure_tcp: false,
        max_rpc_bytes: nova_router::DEFAULT_MAX_RPC_BYTES,
        max_inflight_handshakes: nova_router::DEFAULT_MAX_INFLIGHT_HANDSHAKES,
        max_worker_connections: nova_router::DEFAULT_MAX_WORKER_CONNECTIONS,
        #[cfg(feature = "tls")]
        tls_client_cert_fingerprint_allowlist: Default::default(),
        spawn_workers: false,
    };
    let layout = WorkspaceLayout {
        source_roots: vec![SourceRoot { path: shard0 }, SourceRoot { path: shard1 }],
    };
    let router = QueryRouter::new_distributed(config, layout).await?;

    let (ready_tx, ready_rx) = oneshot::channel();
    let worker_task = tokio::spawn(async move {
        let conn = connect_worker(&listen_path, 0).await?;
        let _ = ready_tx.send(());

        match conn {
            remote_rpc_util::ConnectedWorker::LegacyV2(mut stream) => {
                let msg = read_rpc(&mut stream).await?;
                let revision = match msg {
                    RpcMessage::UpdateFile { revision, .. } => revision,
                    other => return Err(anyhow!("expected UpdateFile, got {other:?}")),
                };

                write_rpc(
                    &mut stream,
                    &RpcMessage::ShardIndexInfo(ShardIndexInfo {
                        shard_id: 1,
                        revision,
                        index_generation: 1,
                        symbol_count: 0,
                    }),
                )
                .await?;

                expect_disconnect(stream).await
            }
            remote_rpc_util::ConnectedWorker::V3(conn) => {
                let (handled_tx, mut handled_rx) = tokio::sync::watch::channel(false);
                let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
                conn.set_request_handler(move |_ctx, req| {
                    let handled_tx = handled_tx.clone();
                    let shutdown_tx = shutdown_tx.clone();
                    async move {
                        match req {
                            Request::UpdateFile { revision, .. } => {
                                let _ = handled_tx.send(true);
                                Ok(Response::ShardIndex(ShardIndex {
                                    shard_id: 1,
                                    revision,
                                    index_generation: 1,
                                    symbols: Vec::new(),
                                }))
                            }
                            Request::Shutdown => {
                                let _ = shutdown_tx.send(true);
                                Ok(Response::Shutdown)
                            }
                            _ => Ok(Response::Ack),
                        }
                    }
                });

                tokio::time::timeout(Duration::from_secs(1), async {
                    while handled_rx.changed().await.is_ok() {
                        if *handled_rx.borrow() {
                            break;
                        }
                    }
                })
                .await
                .context("timed out waiting for router request")?;

                expect_disconnect_v3(conn, shutdown_rx, Duration::from_secs(1)).await
            }
        }
    });

    ready_rx
        .await
        .context("worker did not complete handshake")?;

    let res = router
        .update_file(
            file0,
            "package a; public class Alpha {} class Gamma {}".into(),
        )
        .await;
    assert!(
        res.is_err(),
        "expected router to reject mismatched shard index"
    );

    tokio::time::timeout(Duration::from_secs(2), worker_task)
        .await
        .context("worker connection not closed")?
        .context("worker task failed")??;

    router.shutdown().await?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn worker_stats_rejects_mismatched_shard_id() -> Result<()> {
    let tmp = TempDir::new()?;
    let workspace_root = tmp.path();

    let shard0 = workspace_root.join("shard0").join("src");
    tokio::fs::create_dir_all(&shard0).await?;

    let listen_path = workspace_root.join("router.sock");
    let cache_dir = workspace_root.join("cache");

    let config = DistributedRouterConfig {
        listen_addr: ListenAddr::Unix(listen_path.clone()),
        worker_command: PathBuf::from("unused-worker"),
        cache_dir,
        auth_token: None,
        allow_insecure_tcp: false,
        max_rpc_bytes: nova_router::DEFAULT_MAX_RPC_BYTES,
        max_inflight_handshakes: nova_router::DEFAULT_MAX_INFLIGHT_HANDSHAKES,
        max_worker_connections: nova_router::DEFAULT_MAX_WORKER_CONNECTIONS,
        #[cfg(feature = "tls")]
        tls_client_cert_fingerprint_allowlist: Default::default(),
        spawn_workers: false,
    };
    let layout = WorkspaceLayout {
        source_roots: vec![SourceRoot { path: shard0 }],
    };
    let router = QueryRouter::new_distributed(config, layout).await?;

    let (ready_tx, ready_rx) = oneshot::channel();
    let worker_task = tokio::spawn(async move {
        let conn = connect_worker(&listen_path, 0).await?;
        let _ = ready_tx.send(());

        match conn {
            remote_rpc_util::ConnectedWorker::LegacyV2(mut stream) => {
                let msg = read_rpc(&mut stream).await?;
                match msg {
                    RpcMessage::GetWorkerStats => {}
                    other => return Err(anyhow!("expected GetWorkerStats, got {other:?}")),
                }

                write_rpc(
                    &mut stream,
                    &RpcMessage::WorkerStats(WorkerStats {
                        shard_id: 1,
                        revision: 0,
                        index_generation: 0,
                        file_count: 0,
                    }),
                )
                .await?;

                expect_disconnect(stream).await
            }
            remote_rpc_util::ConnectedWorker::V3(conn) => {
                let (handled_tx, mut handled_rx) = tokio::sync::watch::channel(false);
                let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
                conn.set_request_handler(move |_ctx, req| {
                    let handled_tx = handled_tx.clone();
                    let shutdown_tx = shutdown_tx.clone();
                    async move {
                        match req {
                            Request::GetWorkerStats => {
                                let _ = handled_tx.send(true);
                                Ok(Response::WorkerStats(WorkerStats {
                                    shard_id: 1,
                                    revision: 0,
                                    index_generation: 0,
                                    file_count: 0,
                                }))
                            }
                            Request::Shutdown => {
                                let _ = shutdown_tx.send(true);
                                Ok(Response::Shutdown)
                            }
                            _ => Ok(Response::Ack),
                        }
                    }
                });

                tokio::time::timeout(Duration::from_secs(1), async {
                    while handled_rx.changed().await.is_ok() {
                        if *handled_rx.borrow() {
                            break;
                        }
                    }
                })
                .await
                .context("timed out waiting for router request")?;

                expect_disconnect_v3(conn, shutdown_rx, Duration::from_secs(1)).await
            }
        }
    });

    ready_rx
        .await
        .context("worker did not complete handshake")?;

    let res = router.worker_stats().await;
    assert!(
        res.is_err(),
        "expected router to reject mismatched worker stats"
    );

    tokio::time::timeout(Duration::from_secs(2), worker_task)
        .await
        .context("worker connection not closed")?
        .context("worker task failed")??;

    router.shutdown().await?;
    Ok(())
}
