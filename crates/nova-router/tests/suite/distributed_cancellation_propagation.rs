use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use anyhow::Context;
use nova_router::{DistributedRouterConfig, ListenAddr, QueryRouter, SourceRoot, WorkspaceLayout};
use nova_scheduler::CancellationToken;
use tempfile::TempDir;
use tokio::time::{timeout, Duration, Instant};

// These tests spawn routers + external worker processes and can be flaky when the Rust test
// harness runs them concurrently (the default on multi-core machines).
//
// Serialize them to keep timings predictable and prevent cross-test resource contention.
static CANCELLATION_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn distributed_rpc_cancellation_propagates_to_worker() -> anyhow::Result<()> {
    let _guard = CANCELLATION_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap();

    let tmp = TempDir::new()?;
    let workspace_root = tmp.path();

    let source_root = workspace_root.join("module_a").join("src");
    tokio::fs::create_dir_all(&source_root).await?;
    tokio::fs::write(
        source_root.join("A.java"),
        "package a; public class Alpha {}",
    )
    .await?;

    let listen_path = workspace_root.join("router.sock");
    let cache_dir = workspace_root.join("cache");
    tokio::fs::create_dir_all(&cache_dir).await?;
    tokio::fs::write(
        cache_dir.join("nova-router-test-worker.conf"),
        "block_index_until_cancel=true\n",
    )
    .await?;

    let worker_bin = PathBuf::from(env!("CARGO_BIN_EXE_nova-router-test-worker"));

    let config = DistributedRouterConfig {
        listen_addr: ListenAddr::Unix(listen_path),
        worker_command: worker_bin,
        cache_dir: cache_dir.clone(),
        auth_token: None,
        allow_insecure_tcp: false,
        max_rpc_bytes: nova_router::DEFAULT_MAX_RPC_BYTES,
        max_inflight_handshakes: nova_router::DEFAULT_MAX_INFLIGHT_HANDSHAKES,
        max_worker_connections: nova_router::DEFAULT_MAX_WORKER_CONNECTIONS,
        #[cfg(feature = "tls")]
        tls_client_cert_fingerprint_allowlist: Default::default(),
        spawn_workers: true,
    };

    let layout = WorkspaceLayout {
        source_roots: vec![SourceRoot { path: source_root }],
    };
    let router = QueryRouter::new_distributed(config, layout).await?;

    // Ensure the worker is connected before starting the cancellation test.
    let stats = router.worker_stats().await?;
    assert!(stats.contains_key(&0));

    let cancel = CancellationToken::new();
    let started_marker = cache_dir.join("index-started-shard0.marker");
    let cancelled_marker = cache_dir.join("index-cancelled-shard0.marker");

    let cancel_for_task = cancel.clone();
    let (cancelled_at_tx, cancelled_at_rx) = tokio::sync::oneshot::channel();
    let cancel_task = tokio::spawn(async move {
        timeout(Duration::from_secs(10), async {
            loop {
                if tokio::fs::metadata(&started_marker).await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .context("timed out waiting for worker to start IndexShard request")?;

        cancel_for_task.cancel();
        let _ = cancelled_at_tx.send(Instant::now());
        Ok::<(), anyhow::Error>(())
    });

    let call_result = timeout(
        Duration::from_secs(15),
        router.index_workspace_cancelable(cancel),
    )
    .await
    .context("index_workspace_cancelable timed out")?;
    let call_finished_at = Instant::now();

    cancel_task.await.context("cancellation task panicked")??;

    let cancelled_at = cancelled_at_rx
        .await
        .context("cancellation task did not report cancellation time")?;

    let cancel_to_return = call_finished_at.duration_since(cancelled_at);
    assert!(
        cancel_to_return < Duration::from_secs(1),
        "router did not return promptly after cancellation: {cancel_to_return:?}"
    );

    let err = call_result.expect_err("expected cancellation error");
    assert!(
        err.downcast_ref::<nova_remote_rpc::RpcError>()
            .is_some_and(|err| matches!(err, nova_remote_rpc::RpcError::Canceled)),
        "expected RpcError::Canceled, got {err:?}"
    );

    // The worker should observe the Cancel packet and mark that it saw cancellation.
    timeout(Duration::from_secs(2), async {
        loop {
            if tokio::fs::metadata(&cancelled_marker).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("timed out waiting for worker cancellation marker")?;

    router.shutdown().await?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn distributed_indexing_starts_all_shards_concurrently() -> anyhow::Result<()> {
    let _guard = CANCELLATION_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap();

    let tmp = TempDir::new()?;
    let workspace_root = tmp.path();

    let source_root_a = workspace_root.join("module_a").join("src");
    tokio::fs::create_dir_all(&source_root_a).await?;
    tokio::fs::write(
        source_root_a.join("A.java"),
        "package a; public class Alpha {}",
    )
    .await?;

    let source_root_b = workspace_root.join("module_b").join("src");
    tokio::fs::create_dir_all(&source_root_b).await?;
    tokio::fs::write(
        source_root_b.join("B.java"),
        "package b; public class Beta {}",
    )
    .await?;

    let listen_path = workspace_root.join("router.sock");
    let cache_dir = workspace_root.join("cache");
    tokio::fs::create_dir_all(&cache_dir).await?;
    tokio::fs::write(
        cache_dir.join("nova-router-test-worker.conf"),
        "block_index_until_cancel=true\n",
    )
    .await?;

    let worker_bin = PathBuf::from(env!("CARGO_BIN_EXE_nova-router-test-worker"));

    let config = DistributedRouterConfig {
        listen_addr: ListenAddr::Unix(listen_path),
        worker_command: worker_bin,
        cache_dir: cache_dir.clone(),
        auth_token: None,
        allow_insecure_tcp: false,
        max_rpc_bytes: nova_router::DEFAULT_MAX_RPC_BYTES,
        max_inflight_handshakes: nova_router::DEFAULT_MAX_INFLIGHT_HANDSHAKES,
        max_worker_connections: nova_router::DEFAULT_MAX_WORKER_CONNECTIONS,
        #[cfg(feature = "tls")]
        tls_client_cert_fingerprint_allowlist: Default::default(),
        spawn_workers: true,
    };

    let layout = WorkspaceLayout {
        source_roots: vec![
            SourceRoot {
                path: source_root_a,
            },
            SourceRoot {
                path: source_root_b,
            },
        ],
    };
    let router = QueryRouter::new_distributed(config, layout).await?;

    // Ensure both workers are connected before starting the concurrency test.
    let stats = timeout(Duration::from_secs(10), router.worker_stats())
        .await
        .context("worker_stats timed out")??;
    assert!(stats.contains_key(&0));
    assert!(stats.contains_key(&1));

    let cancel = CancellationToken::new();
    let started_marker_0 = cache_dir.join("index-started-shard0.marker");
    let started_marker_1 = cache_dir.join("index-started-shard1.marker");
    let cancelled_marker_0 = cache_dir.join("index-cancelled-shard0.marker");
    let cancelled_marker_1 = cache_dir.join("index-cancelled-shard1.marker");

    let cancel_for_task = cancel.clone();
    let (cancelled_at_tx, cancelled_at_rx) = tokio::sync::oneshot::channel();
    let cancel_task = tokio::spawn(async move {
        timeout(Duration::from_secs(10), async {
            loop {
                let started_0 = tokio::fs::metadata(&started_marker_0).await.is_ok();
                let started_1 = tokio::fs::metadata(&started_marker_1).await.is_ok();
                if started_0 && started_1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .context("timed out waiting for workers to start IndexShard requests")?;

        cancel_for_task.cancel();
        let _ = cancelled_at_tx.send(Instant::now());
        Ok::<(), anyhow::Error>(())
    });

    let call_result = timeout(
        Duration::from_secs(15),
        router.index_workspace_cancelable(cancel),
    )
    .await
    .context("index_workspace_cancelable timed out")?;
    let call_finished_at = Instant::now();

    cancel_task.await.context("cancellation task panicked")??;

    let cancelled_at = cancelled_at_rx
        .await
        .context("cancellation task did not report cancellation time")?;

    let cancel_to_return = call_finished_at.duration_since(cancelled_at);
    assert!(
        cancel_to_return < Duration::from_secs(1),
        "router did not return promptly after cancellation: {cancel_to_return:?}"
    );

    let err = call_result.expect_err("expected cancellation error");
    assert!(
        err.downcast_ref::<nova_remote_rpc::RpcError>()
            .is_some_and(|err| matches!(err, nova_remote_rpc::RpcError::Canceled)),
        "expected RpcError::Canceled, got {err:?}"
    );

    timeout(Duration::from_secs(2), async {
        loop {
            let cancelled_0 = tokio::fs::metadata(&cancelled_marker_0).await.is_ok();
            let cancelled_1 = tokio::fs::metadata(&cancelled_marker_1).await.is_ok();
            if cancelled_0 && cancelled_1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("timed out waiting for worker cancellation markers")?;

    router.shutdown().await?;
    Ok(())
}
