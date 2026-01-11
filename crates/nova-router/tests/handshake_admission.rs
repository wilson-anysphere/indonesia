use std::path::PathBuf;
use std::time::Duration;

use nova_router::{
    DistributedRouterConfig, ListenAddr, QueryRouter, SourceRoot, TcpListenAddr, WorkspaceLayout,
};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{timeout, Instant};

mod remote_rpc_util;

async fn complete_handshake(addr: std::net::SocketAddr) -> anyhow::Result<()> {
    let conn = remote_rpc_util::connect_and_handshake_worker(
        || async { Ok(TcpStream::connect(addr).await.map_err(anyhow::Error::from)?) },
        0,
        None,
    )
    .await?;
    conn.shutdown().await;
    Ok(())
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
    complete_handshake(addr).await?;

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

    complete_handshake(addr).await?;

    router.shutdown().await?;
    Ok(())
}
