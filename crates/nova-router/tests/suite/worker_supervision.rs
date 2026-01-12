use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};

use anyhow::Context;
use nova_router::{DistributedRouterConfig, ListenAddr, QueryRouter, SourceRoot, WorkspaceLayout};
use tempfile::TempDir;
use tokio::process::Command;
use tokio::time::{timeout, Duration, Instant};

// These tests spawn routers + external worker processes and can be flaky when the Rust test
// harness runs them concurrently (the default on multi-core machines).
//
// Serialize them to keep timings predictable and prevent cross-test resource contention.
static WORKER_SUPERVISION_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn worker_supervisor_backs_off_on_crash_loop_and_recovers() -> anyhow::Result<()> {
    let _guard = WORKER_SUPERVISION_TEST_LOCK
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
        "fail_attempts=3\n",
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

    let stats = router.worker_stats().await?;
    assert!(stats.contains_key(&0));

    let log_path = cache_dir.join("attempts-shard0.log");
    let log = tokio::fs::read_to_string(&log_path).await?;
    let times: Vec<u128> = log
        .lines()
        .filter_map(|line| line.trim().parse::<u128>().ok())
        .collect();

    assert!(
        times.len() >= 4,
        "expected at least 4 worker starts (3 failures + 1 success), got {}",
        times.len()
    );

    let delta1 = times[1].saturating_sub(times[0]);
    let delta2 = times[2].saturating_sub(times[1]);
    let delta3 = times[3].saturating_sub(times[2]);

    assert!(delta1 >= 40, "expected backoff >= ~50ms, got {delta1}ms");
    assert!(delta2 >= 80, "expected backoff >= ~100ms, got {delta2}ms");
    assert!(delta3 >= 160, "expected backoff >= ~200ms, got {delta3}ms");

    router.shutdown().await?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn worker_supervisor_enforces_handshake_deadline() -> anyhow::Result<()> {
    let _guard = WORKER_SUPERVISION_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap();
    let tmp = TempDir::new()?;
    let workspace_root = tmp.path();

    let source_root = workspace_root.join("module_a").join("src");
    tokio::fs::create_dir_all(&source_root).await?;

    let listen_path = workspace_root.join("router.sock");
    let cache_dir = workspace_root.join("cache");
    tokio::fs::create_dir_all(&cache_dir).await?;
    tokio::fs::write(
        cache_dir.join("nova-router-test-worker.conf"),
        "connect_delay_ms=6000\nconnect_delay_attempts=1\n",
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

    let stats = router.worker_stats().await?;
    assert!(stats.contains_key(&0));

    let log_path = cache_dir.join("attempts-shard0.log");
    let log = tokio::fs::read_to_string(&log_path).await?;
    let times: Vec<u128> = log
        .lines()
        .filter_map(|line| line.trim().parse::<u128>().ok())
        .collect();

    assert!(
        times.len() >= 2,
        "expected at least 2 worker starts due to handshake timeout, got {}",
        times.len()
    );

    let delta = times[1].saturating_sub(times[0]);
    assert!(
        delta >= 4_800,
        "expected restart delay to include ~5s handshake timeout; got {delta}ms"
    );

    router.shutdown().await?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn worker_supervisor_recovers_when_worker_exits_while_idle() -> anyhow::Result<()> {
    let _guard = WORKER_SUPERVISION_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap();
    let tmp = TempDir::new()?;
    let workspace_root = tmp.path();

    let source_root = workspace_root.join("module_a").join("src");
    tokio::fs::create_dir_all(&source_root).await?;

    let listen_path = workspace_root.join("router.sock");
    let cache_dir = workspace_root.join("cache");
    tokio::fs::create_dir_all(&cache_dir).await?;
    tokio::fs::write(
        cache_dir.join("nova-router-test-worker.conf"),
        "exit_after_handshake_attempts=1\n",
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

    let count_path = cache_dir.join("attempts-shard0.count");
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut attempt_count = 0u32;
    loop {
        if Instant::now() >= deadline {
            anyhow::bail!(
                "timed out waiting for worker restart; last attempt count: {attempt_count}"
            );
        }

        if let Ok(contents) = tokio::fs::read_to_string(&count_path).await {
            if let Ok(count) = contents.trim().parse::<u32>() {
                attempt_count = count;
            }
        }

        if attempt_count >= 2 {
            break;
        }

        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    tokio::time::sleep(Duration::from_millis(300)).await;
    let final_count: u32 = tokio::fs::read_to_string(&count_path)
        .await?
        .trim()
        .parse()
        .unwrap_or(attempt_count);
    assert!(
        final_count <= 3,
        "expected worker restart to settle after initial exit; got {final_count} attempts"
    );

    let stats = timeout(Duration::from_secs(5), router.worker_stats()).await??;
    assert!(stats.contains_key(&0));

    router.shutdown().await?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn router_accepts_replacement_worker_after_remote_disconnect() -> anyhow::Result<()> {
    let _guard = WORKER_SUPERVISION_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap();
    let tmp = TempDir::new()?;
    let workspace_root = tmp.path();

    let source_root = workspace_root.join("module_a").join("src");
    tokio::fs::create_dir_all(&source_root).await?;

    let listen_path = workspace_root.join("router.sock");
    let cache_dir = workspace_root.join("cache");
    tokio::fs::create_dir_all(&cache_dir).await?;
    tokio::fs::write(
        cache_dir.join("nova-router-test-worker.conf"),
        "exit_after_handshake_attempts=1\n",
    )
    .await?;

    let worker_bin = PathBuf::from(env!("CARGO_BIN_EXE_nova-router-test-worker"));

    let config = DistributedRouterConfig {
        listen_addr: ListenAddr::Unix(listen_path.clone()),
        // Not used because spawn_workers is false (we spawn the fixture workers ourselves).
        worker_command: PathBuf::from("unused-worker-bin"),
        cache_dir: cache_dir.clone(),
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
        source_roots: vec![SourceRoot { path: source_root }],
    };
    let router = QueryRouter::new_distributed(config, layout).await?;

    let deadline = Instant::now() + Duration::from_secs(2);
    while std::fs::metadata(&listen_path).is_err() {
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for router socket {listen_path:?} to be created");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let connect_arg = format!("unix:{}", listen_path.display());

    let mut first = Command::new(&worker_bin);
    first
        .kill_on_drop(true)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .arg("--connect")
        .arg(&connect_arg)
        .arg("--shard-id")
        .arg("0")
        .arg("--cache-dir")
        .arg(&cache_dir);
    let mut first = first.spawn()?;
    let status = timeout(Duration::from_secs(10), first.wait())
        .await
        .context("timed out waiting for first worker to exit")??;
    assert!(
        status.success(),
        "expected first worker to exit cleanly, got {status:?}"
    );

    // Give the router time to observe the disconnect and clear `shard.worker`.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut second = Command::new(&worker_bin);
    second
        .kill_on_drop(true)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .arg("--connect")
        .arg(&connect_arg)
        .arg("--shard-id")
        .arg("0")
        .arg("--cache-dir")
        .arg(&cache_dir);
    let mut second = second.spawn()?;

    let stats = timeout(Duration::from_secs(10), router.worker_stats())
        .await
        .context("timed out waiting for replacement worker to respond to worker_stats")??;
    assert!(stats.contains_key(&0));

    router.shutdown().await?;
    let _ = timeout(Duration::from_secs(5), second.wait()).await?;
    Ok(())
}
