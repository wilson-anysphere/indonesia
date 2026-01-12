use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use crate::remote_rpc_util;
use nova_remote_proto::v3::Notification;
use nova_remote_proto::ShardIndex;
use nova_router::{
    DistributedRouterConfig, ListenAddr, QueryRouter, SourceRoot, TcpListenAddr, WorkspaceLayout,
};
use tokio::net::TcpStream;
use tokio::time::timeout;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cached_index_notification_for_wrong_shard_disconnects_worker() -> Result<()> {
    let tmp = tempfile::tempdir().context("create temp dir")?;
    let cache_dir = tmp.path().join("cache");
    tokio::fs::create_dir_all(&cache_dir)
        .await
        .context("create cache dir")?;

    let root = tmp.path().join("root");
    tokio::fs::create_dir_all(&root)
        .await
        .context("create source root")?;

    let config = DistributedRouterConfig {
        listen_addr: ListenAddr::Tcp(TcpListenAddr::Plain("127.0.0.1:0".parse()?)),
        worker_command: PathBuf::from("unused"),
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
        source_roots: vec![SourceRoot { path: root }],
    };
    let router = QueryRouter::new_distributed(config, layout)
        .await
        .context("start distributed router")?;

    let listen = timeout(Duration::from_secs(2), router.bound_listen_addr())
        .await
        .map_err(|_| anyhow!("timed out waiting for router to bind"))?
        .ok_or_else(|| anyhow!("router did not report a bound listen address"))?;
    let addr: SocketAddr = match listen {
        ListenAddr::Tcp(TcpListenAddr::Plain(addr)) => addr,
        other => return Err(anyhow!("unexpected listen address: {other:?}")),
    };

    let conn = remote_rpc_util::connect_and_handshake_worker(
        || async { Ok(TcpStream::connect(addr).await?) },
        0,
        None,
    )
    .await?;

    let worker_conn = match conn {
        remote_rpc_util::ConnectedWorker::V3(conn) => conn,
        remote_rpc_util::ConnectedWorker::LegacyV2(_) => {
            return Err(anyhow!("expected v3 router; got legacy_v2 connection"))
        }
    };

    // Send a CachedIndex notification claiming a different shard. The router should treat this as
    // a protocol violation and disconnect the worker.
    let bad_index = ShardIndex {
        shard_id: 1,
        revision: 0,
        index_generation: 0,
        symbols: Vec::new(),
    };
    let _ = worker_conn
        .notify(Notification::CachedIndex(bad_index))
        .await;

    let _ = timeout(Duration::from_secs(2), worker_conn.wait_closed())
        .await
        .context("timed out waiting for router to disconnect worker")?;

    // The shard reservation should be cleared promptly so a replacement worker can connect.
    let replacement = remote_rpc_util::connect_and_handshake_worker(
        || async { Ok(TcpStream::connect(addr).await?) },
        0,
        None,
    )
    .await?;
    replacement.shutdown().await;

    router.shutdown().await?;
    Ok(())
}

