use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::path::PathBuf;

use anyhow::{Context, Result};
use crate::remote_rpc_util;
use nova_router::{
    DistributedRouterConfig, ListenAddr, QueryRouter, SourceRoot, TcpListenAddr, WorkspaceLayout,
};
use tokio::net::TcpStream;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_worker_connections_for_same_shard_are_rejected() -> Result<()> {
    let tmp = tempfile::tempdir().context("create temp dir")?;
    let root = tmp.path().join("root");
    tokio::fs::create_dir_all(&root)
        .await
        .context("create source root")?;

    let addr = reserve_tcp_addr()?;
    let config = DistributedRouterConfig {
        listen_addr: ListenAddr::Tcp(TcpListenAddr::Plain(addr)),
        worker_command: PathBuf::from("unused"),
        cache_dir: tmp.path().join("cache"),
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
        source_roots: vec![SourceRoot { path: root }],
    };
    let router = QueryRouter::new_distributed(config, layout)
        .await
        .context("start router")?;

    let f1 = connect_and_handshake(addr);
    let f2 = connect_and_handshake(addr);
    let (r1, r2) = tokio::try_join!(f1, f2)?;

    let mut successes = Vec::new();
    let mut errors = Vec::new();
    for res in [r1, r2] {
        match res {
            Ok(conn) => successes.push(conn),
            Err(err) => errors.push(err),
        }
    }

    assert_eq!(
        successes.len(),
        1,
        "expected exactly one successful worker handshake, got {} (errors: {errors:?})",
        successes.len()
    );
    assert_eq!(
        errors.len(),
        1,
        "expected exactly one rejected worker handshake, got {}",
        errors.len()
    );
    assert!(
        errors[0].contains("already has") || errors[0].contains("connected worker"),
        "unexpected handshake rejection message: {:?}",
        errors[0]
    );

    for conn in successes {
        conn.shutdown().await;
    }

    router.shutdown().await.context("shutdown router")?;
    Ok(())
}

async fn connect_and_handshake(
    addr: SocketAddr,
) -> Result<std::result::Result<remote_rpc_util::ConnectedWorker<TcpStream>, String>> {
    let res = remote_rpc_util::connect_and_handshake_worker(
        || async { connect_with_retries(addr).await },
        0,
        None,
    )
    .await;
    Ok(match res {
        Ok(conn) => Ok(conn),
        Err(err) => Err(err.to_string()),
    })
}

fn reserve_tcp_addr() -> Result<SocketAddr> {
    let listener = StdTcpListener::bind("127.0.0.1:0").context("bind tcp listener")?;
    let addr = listener.local_addr().context("get local_addr")?;
    drop(listener);
    Ok(addr)
}

async fn connect_with_retries(addr: SocketAddr) -> Result<TcpStream> {
    let mut attempts = 0u32;
    loop {
        match TcpStream::connect(addr).await {
            Ok(stream) => return Ok(stream),
            Err(err) if attempts < 50 => {
                attempts += 1;
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                continue;
            }
            Err(err) => return Err(err).with_context(|| format!("connect to router {addr}")),
        }
    }
}
