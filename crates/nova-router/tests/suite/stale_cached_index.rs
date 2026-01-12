use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use nova_remote_proto::v3::{
    CachedIndexInfo, Capabilities, Notification, ProtocolVersion, Request, Response,
    SupportedVersions, WorkerHello,
};
use nova_remote_proto::{ShardIndex, Symbol};
use nova_remote_rpc::RpcConnection;
use nova_router::{
    DistributedRouterConfig, ListenAddr, QueryRouter, SourceRoot, TcpListenAddr, WorkspaceLayout,
};
use tokio::net::TcpStream;
use tokio::time::timeout;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delayed_cached_index_does_not_overwrite_newer_index() -> Result<()> {
    let tmp = tempfile::tempdir().context("create temp dir")?;
    let cache_dir = tmp.path().join("cache");
    tokio::fs::create_dir_all(&cache_dir)
        .await
        .context("create cache dir")?;

    let root = tmp.path().join("root");
    tokio::fs::create_dir_all(&root)
        .await
        .context("create source root")?;
    tokio::fs::write(root.join("A.java"), "package a; public class Alpha {}")
        .await
        .context("write fixture java file")?;

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

    let hello = WorkerHello {
        shard_id: 0,
        auth_token: None,
        supported_versions: SupportedVersions {
            min: ProtocolVersion::CURRENT,
            max: ProtocolVersion::CURRENT,
        },
        capabilities: Capabilities {
            supports_cancel: true,
            supports_chunking: true,
            ..Capabilities::default()
        },
        // Mark this worker as having a cached index so the router will accept CachedIndex
        // notifications and run the post-handshake refresh path.
        cached_index_info: Some(CachedIndexInfo {
            revision: 0,
            index_generation: 1,
            symbol_count: 1,
        }),
        worker_build: None,
    };

    let (worker_conn, welcome) =
        RpcConnection::handshake_as_worker(TcpStream::connect(addr).await?, hello)
            .await
            .context("handshake as v3 worker")?;
    anyhow::ensure!(welcome.shard_id == 0, "welcome shard mismatch");

    let fresh_symbol = Symbol {
        name: "FreshSymbol".into(),
        path: "fresh.java".into(),
        line: 0,
        column: 0,
    };
    let stale_symbol = Symbol {
        name: "StaleSymbol".into(),
        path: "stale.java".into(),
        line: 0,
        column: 0,
    };
    let stale_symbol_generation = Symbol {
        name: "StaleSymbolGeneration".into(),
        path: "stale_gen.java".into(),
        line: 0,
        column: 0,
    };

    worker_conn.set_request_handler({
        let fresh_symbol = fresh_symbol.clone();
        move |_ctx, req| {
            let fresh_symbol = fresh_symbol.clone();
            async move {
                match req {
                    Request::LoadFiles { .. } => Ok(Response::Ack),
                    Request::IndexShard { revision, .. } => Ok(Response::ShardIndex(ShardIndex {
                        shard_id: 0,
                        revision,
                        index_generation: 2,
                        symbols: vec![fresh_symbol],
                    })),
                    Request::Shutdown => Ok(Response::Shutdown),
                    _ => Ok(Response::Ack),
                }
            }
        }
    });

    router.index_workspace().await.context("index workspace")?;
    let expected_symbols = vec![fresh_symbol.clone()];
    assert_eq!(
        router.workspace_symbols("").await,
        expected_symbols,
        "fresh symbol should be present after IndexShard"
    );

    // Simulate a delayed cached index notification arriving after a newer index has already been
    // applied. Without monotonic `(revision, index_generation)` gating, this would overwrite the
    // fresh index and regress workspace symbols.
    worker_conn
        .notify(Notification::CachedIndex(ShardIndex {
            shard_id: 0,
            // Stale by revision (even if a buggy implementation looked only at generation).
            revision: 0,
            index_generation: 999,
            symbols: vec![stale_symbol],
        }))
        .await
        .context("send CachedIndex notification (stale by revision)")?;

    // Also validate the generation tie-breaker for the same revision.
    worker_conn
        .notify(Notification::CachedIndex(ShardIndex {
            shard_id: 0,
            revision: 1,
            index_generation: 1,
            symbols: vec![stale_symbol_generation],
        }))
        .await
        .context("send CachedIndex notification (stale by generation)")?;

    // Ensure the router has an opportunity to process the notifications. On the buggy behavior
    // this should quickly flip workspace symbols to one of the stale symbols.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    while tokio::time::Instant::now() < deadline {
        assert_eq!(
            router.workspace_symbols("").await,
            expected_symbols,
            "stale cached index should not overwrite newer index"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    router.shutdown().await?;
    Ok(())
}
