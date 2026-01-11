#![cfg(unix)]

use std::path::PathBuf;
use std::time::Duration;

use nova_router::{DistributedRouterConfig, ListenAddr, QueryRouter, SourceRoot, WorkspaceLayout};

mod remote_rpc_util;

#[tokio::test]
async fn unix_socket_enforces_auth_token_when_configured() -> anyhow::Result<()> {
    use tokio::net::UnixStream;

    let tmp = tempfile::TempDir::new()?;
    let dir = tmp.path();

    let socket_path = dir.join("router.sock");
    let cache_dir = dir.join("cache");
    tokio::fs::create_dir_all(&cache_dir).await?;

    let root = dir.join("root");
    tokio::fs::create_dir_all(&root).await?;

    let config = DistributedRouterConfig {
        listen_addr: ListenAddr::Unix(socket_path.clone()),
        worker_command: PathBuf::from("unused-worker-bin"),
        cache_dir,
        auth_token: Some("secret-token".into()),
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
    let router = QueryRouter::new_distributed(config, layout).await?;

    // Wait for the socket path to exist.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while std::fs::metadata(&socket_path).is_err() {
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for router socket {socket_path:?} to be created");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Connection without auth token should be rejected.
    let res = remote_rpc_util::connect_and_handshake_worker(
        || async {
            Ok(UnixStream::connect(&socket_path)
                .await
                .map_err(anyhow::Error::from)?)
        },
        0,
        None,
    )
    .await;
    let err = match res {
        Ok(conn) => {
            conn.shutdown().await;
            anyhow::bail!("expected unauthenticated worker to be rejected");
        }
        Err(err) => err,
    };
    let msg = err.to_string();
    assert!(
        msg.contains("authentication"),
        "unexpected error message: {msg:?}"
    );

    // A correctly-authenticated worker can connect.
    let conn = remote_rpc_util::connect_and_handshake_worker(
        || async {
            Ok(UnixStream::connect(&socket_path)
                .await
                .map_err(anyhow::Error::from)?)
        },
        0,
        Some("secret-token".into()),
    )
    .await?;
    conn.shutdown().await;

    router.shutdown().await?;
    Ok(())
}
