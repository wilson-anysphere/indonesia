#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use nova_remote_proto::RpcMessage;
use nova_router::{DistributedRouterConfig, ListenAddr, QueryRouter, SourceRoot, WorkspaceLayout};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

async fn connect_with_retry(path: &Path) -> Result<UnixStream> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        match UnixStream::connect(path).await {
            Ok(stream) => return Ok(stream),
            Err(err) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(err).with_context(|| format!("connect unix socket {path:?}"));
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }
}

async fn write_rpc_message(stream: &mut UnixStream, msg: &RpcMessage) -> Result<()> {
    let payload = nova_remote_proto::encode_message(msg)?;
    let len: u32 = payload
        .len()
        .try_into()
        .map_err(|_| anyhow!("message too large for u32 length"))?;
    stream.write_u32_le(len).await.context("write frame len")?;
    stream
        .write_all(&payload)
        .await
        .context("write frame payload")?;
    stream.flush().await.context("flush frame")?;
    Ok(())
}

async fn read_rpc_message(stream: &mut UnixStream) -> Result<RpcMessage> {
    let len = stream.read_u32_le().await.context("read frame len")?;
    let mut buf = vec![0u8; len as usize];
    stream
        .read_exact(&mut buf)
        .await
        .context("read frame payload")?;
    nova_remote_proto::decode_message(&buf)
}

async fn start_router(tmp: &TempDir, listen_path: PathBuf) -> Result<QueryRouter> {
    // Ensure tests don't inherit a restrictive global frame limit from the surrounding
    // environment.
    std::env::set_var(
        "NOVA_RPC_MAX_MESSAGE_SIZE",
        nova_remote_proto::MAX_MESSAGE_BYTES.to_string(),
    );

    let source_root = tmp.path().join("root");
    tokio::fs::create_dir_all(&source_root)
        .await
        .context("create source root dir")?;

    let layout = WorkspaceLayout {
        source_roots: vec![SourceRoot { path: source_root }],
    };

    let config = DistributedRouterConfig {
        listen_addr: ListenAddr::Unix(listen_path),
        worker_command: PathBuf::from("nova-worker"),
        cache_dir: tmp.path().join("cache"),
        auth_token: None,
        allow_insecure_tcp: false,
        #[cfg(feature = "tls")]
        tls_client_cert_fingerprint_allowlist: Default::default(),
        spawn_workers: false,
    };

    QueryRouter::new_distributed(config, layout).await
}

#[tokio::test]
async fn oversized_frame_len_is_rejected_without_killing_accept_loop() -> Result<()> {
    let tmp = TempDir::new()?;
    let listen_path = tmp.path().join("router.sock");
    let router = start_router(&tmp, listen_path.clone()).await?;

    // Send an oversized frame header and then stop. The router should reject it without
    // attempting to read the payload (i.e. without stalling on `read_exact`).
    let mut stream = connect_with_retry(&listen_path).await?;
    stream
        .write_u32_le((nova_remote_proto::MAX_MESSAGE_BYTES as u32) + 1)
        .await
        .context("write oversized len")?;
    stream.flush().await.context("flush oversized len")?;

    let close_res = tokio::time::timeout(Duration::from_secs(2), async {
        let mut buf = [0u8; 1];
        stream.read(&mut buf).await
    })
    .await;
    assert!(
        close_res.is_ok(),
        "router did not close connection promptly after oversized frame"
    );

    // Regression test: invalid connections should not terminate the accept loop.
    let mut stream = connect_with_retry(&listen_path).await?;
    write_rpc_message(
        &mut stream,
        &RpcMessage::WorkerHello {
            shard_id: 0,
            auth_token: None,
            has_cached_index: false,
        },
    )
    .await?;
    let resp = tokio::time::timeout(Duration::from_secs(2), read_rpc_message(&mut stream))
        .await
        .context("timed out waiting for RouterHello")??;

    match resp {
        RpcMessage::RouterHello {
            shard_id,
            protocol_version,
            ..
        } => {
            assert_eq!(shard_id, 0);
            assert_eq!(protocol_version, nova_remote_proto::PROTOCOL_VERSION);
        }
        other => return Err(anyhow!("unexpected router response: {other:?}")),
    }

    router.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn stalled_handshake_does_not_block_other_connections() -> Result<()> {
    let tmp = TempDir::new()?;
    let listen_path = tmp.path().join("router.sock");
    let router = start_router(&tmp, listen_path.clone()).await?;

    // First client connects and never sends the initial hello frame.
    let _stalled = connect_with_retry(&listen_path).await?;

    // Second client should still be able to complete the handshake promptly.
    let mut stream = connect_with_retry(&listen_path).await?;
    write_rpc_message(
        &mut stream,
        &RpcMessage::WorkerHello {
            shard_id: 0,
            auth_token: None,
            has_cached_index: false,
        },
    )
    .await?;

    let resp = tokio::time::timeout(Duration::from_secs(2), read_rpc_message(&mut stream))
        .await
        .context("timed out waiting for RouterHello")??;
    match resp {
        RpcMessage::RouterHello { shard_id, .. } => assert_eq!(shard_id, 0),
        other => return Err(anyhow!("unexpected router response: {other:?}")),
    }

    router.shutdown().await?;
    Ok(())
}
