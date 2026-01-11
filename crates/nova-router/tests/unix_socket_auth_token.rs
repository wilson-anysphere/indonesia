#![cfg(unix)]

use std::path::PathBuf;
use std::time::Duration;

use nova_remote_proto::RpcMessage;
use nova_router::{DistributedRouterConfig, ListenAddr, QueryRouter, SourceRoot, WorkspaceLayout};

#[tokio::test]
async fn unix_socket_enforces_auth_token_when_configured() -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
    let mut stream = UnixStream::connect(&socket_path).await?;
    let hello = nova_remote_proto::encode_message(&RpcMessage::WorkerHello {
        shard_id: 0,
        auth_token: None,
        has_cached_index: false,
    })?;
    stream.write_u32_le(hello.len() as u32).await?;
    stream.write_all(&hello).await?;
    stream.flush().await?;

    let len = stream.read_u32_le().await?;
    let mut buf = vec![0u8; len as usize];
    stream.read_exact(&mut buf).await?;
    let resp = nova_remote_proto::decode_message(&buf)?;
    match resp {
        RpcMessage::Error { message } => {
            assert!(
                message.contains("authentication"),
                "unexpected error message: {message:?}"
            );
        }
        other => anyhow::bail!("expected Error response, got {other:?}"),
    }
    drop(stream);

    // A correctly-authenticated worker can connect.
    let mut stream = UnixStream::connect(&socket_path).await?;
    let hello = nova_remote_proto::encode_message(&RpcMessage::WorkerHello {
        shard_id: 0,
        auth_token: Some("secret-token".into()),
        has_cached_index: false,
    })?;
    stream.write_u32_le(hello.len() as u32).await?;
    stream.write_all(&hello).await?;
    stream.flush().await?;

    let len = stream.read_u32_le().await?;
    let mut buf = vec![0u8; len as usize];
    stream.read_exact(&mut buf).await?;
    let resp = nova_remote_proto::decode_message(&buf)?;
    match resp {
        RpcMessage::RouterHello { shard_id, .. } => assert_eq!(shard_id, 0),
        other => anyhow::bail!("expected RouterHello response, got {other:?}"),
    }

    router.shutdown().await?;
    Ok(())
}
