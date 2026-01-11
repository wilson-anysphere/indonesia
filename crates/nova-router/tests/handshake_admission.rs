use std::path::PathBuf;
use std::time::Duration;

use nova_remote_proto::RpcMessage;
use nova_router::{
    DistributedRouterConfig, ListenAddr, QueryRouter, SourceRoot, TcpListenAddr, WorkspaceLayout,
};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{timeout, Instant};

async fn write_rpc_message(stream: &mut TcpStream, msg: &RpcMessage) -> anyhow::Result<()> {
    let payload = nova_remote_proto::encode_message(msg)?;
    let len: u32 = payload.len().try_into()?;
    stream.write_u32_le(len).await?;
    stream.write_all(&payload).await?;
    stream.flush().await?;
    Ok(())
}

async fn read_rpc_message_limited(
    stream: &mut TcpStream,
    max_len: usize,
) -> anyhow::Result<RpcMessage> {
    let len: usize = stream.read_u32_le().await?.try_into()?;
    anyhow::ensure!(
        len <= max_len,
        "message too large ({len} bytes, max {max_len})"
    );
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    Ok(nova_remote_proto::decode_message(&buf)?)
}

async fn start_router(
    tmp: &TempDir,
    max_inflight_handshakes: usize,
) -> anyhow::Result<(QueryRouter, std::net::SocketAddr)> {
    let root = tmp.path().join("root");
    tokio::fs::create_dir_all(&root).await?;
    let layout = WorkspaceLayout {
        source_roots: vec![SourceRoot { path: root }],
    };

    let config = DistributedRouterConfig {
        listen_addr: ListenAddr::Tcp(TcpListenAddr::Plain("127.0.0.1:0".parse()?)),
        worker_command: PathBuf::from("nova-worker"),
        cache_dir: tmp.path().join("cache"),
        auth_token: None,
        allow_insecure_tcp: false,
        max_rpc_bytes: nova_router::DEFAULT_MAX_RPC_BYTES,
        max_inflight_handshakes,
        max_worker_connections: 128,
        #[cfg(feature = "tls")]
        tls_client_cert_fingerprint_allowlist: Default::default(),
        spawn_workers: false,
    };

    let router = QueryRouter::new_distributed(config, layout).await?;
    let listen = timeout(Duration::from_secs(2), router.bound_listen_addr())
        .await
        .map_err(|_| anyhow::anyhow!("timed out waiting for router to bind"))?
        .ok_or_else(|| anyhow::anyhow!("router did not report a bound listen address"))?;
    let addr = match listen {
        ListenAddr::Tcp(TcpListenAddr::Plain(addr)) => addr,
        other => panic!("expected plain TCP listener, got {other:?}"),
    };

    Ok((router, addr))
}

#[tokio::test]
async fn oversized_worker_hello_is_rejected() -> anyhow::Result<()> {
    let tmp = TempDir::new()?;
    let (router, addr) = start_router(&tmp, 8).await?;

    // Send an absurdly large length prefix without a payload. The router should reject the
    // connection without attempting to allocate the advertised buffer.
    let mut stream = TcpStream::connect(addr).await?;
    stream.write_u32_le(u32::MAX).await?;
    stream.flush().await?;

    let mut buf = [0u8; 1];
    let read_res = timeout(Duration::from_secs(1), stream.read(&mut buf)).await;
    assert!(
        matches!(read_res, Ok(Ok(0)) | Ok(Err(_))),
        "router should close or error on oversized hello (got {read_res:?})"
    );

    // Router should remain healthy and accept a subsequent valid handshake.
    let mut ok = TcpStream::connect(addr).await?;
    write_rpc_message(
        &mut ok,
        &RpcMessage::WorkerHello {
            shard_id: 0,
            auth_token: None,
            has_cached_index: false,
        },
    )
    .await?;
    let ack = read_rpc_message_limited(&mut ok, 64 * 1024).await?;
    match ack {
        RpcMessage::RouterHello { shard_id, .. } => assert_eq!(shard_id, 0),
        other => panic!("expected RouterHello, got {other:?}"),
    }

    router.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn handshake_concurrency_is_bounded() -> anyhow::Result<()> {
    let tmp = TempDir::new()?;
    let (router, addr) = start_router(&tmp, 1).await?;

    // Occupy the single handshake slot by opening a connection and never sending the hello.
    let _stalled = TcpStream::connect(addr).await?;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // With the slot occupied, new connections should be rejected quickly rather than spawning
    // unbounded handshake tasks.
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let mut candidate = TcpStream::connect(addr).await?;
        let mut buf = [0u8; 1];
        match timeout(Duration::from_millis(200), candidate.read(&mut buf)).await {
            Ok(Ok(0)) | Ok(Err(_)) => break,
            Ok(Ok(_n)) => anyhow::bail!("unexpected data from router during handshake"),
            Err(_) => {
                drop(candidate);
                if Instant::now() >= deadline {
                    anyhow::bail!("router did not reject excess handshakes within deadline");
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }

    // Closing the stalled connection should free the slot and allow a new handshake.
    drop(_stalled);
    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut ok = TcpStream::connect(addr).await?;
    write_rpc_message(
        &mut ok,
        &RpcMessage::WorkerHello {
            shard_id: 0,
            auth_token: None,
            has_cached_index: false,
        },
    )
    .await?;
    let ack = read_rpc_message_limited(&mut ok, 64 * 1024).await?;
    assert!(matches!(ack, RpcMessage::RouterHello { .. }));

    router.shutdown().await?;
    Ok(())
}
