use std::path::PathBuf;

use nova_router::{DistributedRouterConfig, ListenAddr, QueryRouter, SourceRoot, WorkspaceLayout};
use tempfile::TempDir;

#[cfg(unix)]
#[tokio::test]
async fn worker_supervisor_backs_off_on_crash_loop_and_recovers() -> anyhow::Result<()> {
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
#[tokio::test]
async fn worker_supervisor_enforces_handshake_deadline() -> anyhow::Result<()> {
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
