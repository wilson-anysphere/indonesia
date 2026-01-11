use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
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
async fn connect_worker(socket_path: &Path, shard_id: ShardId) -> Result<UnixStream> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut last_err = None;
    let mut stream = loop {
        match UnixStream::connect(socket_path).await {
            Ok(stream) => break stream,
            Err(err) => {
                last_err = Some(err);
                if tokio::time::Instant::now() >= deadline {
                    return Err(anyhow!(
                        "timed out connecting to router at {socket_path:?}: {last_err:?}"
                    ));
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    };

    write_rpc(
        &mut stream,
        &RpcMessage::WorkerHello {
            shard_id,
            auth_token: None,
            has_cached_index: false,
        },
    )
    .await?;

    match read_rpc(&mut stream).await? {
        RpcMessage::RouterHello {
            shard_id: ack_shard_id,
            protocol_version,
            ..
        } if ack_shard_id == shard_id
            && protocol_version == nova_remote_proto::PROTOCOL_VERSION => {}
        other => return Err(anyhow!("unexpected RouterHello: {other:?}")),
    }

    Ok(stream)
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
        spawn_workers: false,
    };
    let layout = WorkspaceLayout {
        source_roots: vec![SourceRoot { path: shard0 }, SourceRoot { path: shard1 }],
    };
    let router = QueryRouter::new_distributed(config, layout).await?;

    let (ready_tx, ready_rx) = oneshot::channel();
    let worker_task = tokio::spawn(async move {
        let mut stream = connect_worker(&listen_path, 0).await?;
        let _ = ready_tx.send(());

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
        spawn_workers: false,
    };
    let layout = WorkspaceLayout {
        source_roots: vec![SourceRoot { path: shard0 }],
    };
    let router = QueryRouter::new_distributed(config, layout).await?;

    let (ready_tx, ready_rx) = oneshot::channel();
    let worker_task = tokio::spawn(async move {
        let mut stream = connect_worker(&listen_path, 0).await?;
        let _ = ready_tx.send(());

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
