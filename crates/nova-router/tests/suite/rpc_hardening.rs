#![cfg(unix)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use crate::remote_rpc_util;
use anyhow::{Context, Result};
use nova_router::{DistributedRouterConfig, ListenAddr, QueryRouter, SourceRoot, WorkspaceLayout};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

const MAX_MESSAGE_SIZE_ENV_VAR: &str = "NOVA_RPC_MAX_MESSAGE_SIZE";
static MAX_MESSAGE_SIZE_ENV_LOCK: Mutex<()> = Mutex::new(());

/// A scoped environment variable override.
///
/// Environment variables are process-global, so mutations must be isolated to avoid leaking state
/// across tests once our integration tests are consolidated into a single harness.
struct EnvVarGuard {
    key: &'static str,
    previous: Option<OsString>,
    // Hold a process-wide lock for the duration of the override to avoid racy `set_var`/`remove_var`
    // behavior under `RUST_TEST_THREADS>1`.
    _lock: MutexGuard<'static, ()>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: impl Into<OsString>) -> Self {
        let lock = MAX_MESSAGE_SIZE_ENV_LOCK
            .lock()
            .expect("env lock mutex poisoned");
        let previous = std::env::var_os(key);
        std::env::set_var(key, value.into());
        Self {
            key,
            previous,
            _lock: lock,
        }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}
async fn connect_with_retry(path: &Path) -> Result<UnixStream> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        match UnixStream::connect(path).await {
            Ok(stream) => return Ok(stream),
            Err(err) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(err).with_context(|| format!("connect unix socket {path:?}"));
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }
}

async fn complete_handshake(path: &Path) -> Result<()> {
    let conn = remote_rpc_util::connect_and_handshake_worker(
        || async { connect_with_retry(path).await },
        0,
        None,
    )
    .await?;
    conn.shutdown().await;
    Ok(())
}

async fn start_router(tmp: &TempDir, listen_path: PathBuf) -> Result<(EnvVarGuard, QueryRouter)> {
    // Ensure tests don't inherit a restrictive global frame limit from the surrounding
    // environment.
    let env_guard = EnvVarGuard::set(
        MAX_MESSAGE_SIZE_ENV_VAR,
        nova_remote_proto::MAX_MESSAGE_BYTES.to_string(),
    );

    let source_root = tmp.path().join("root");
    tokio::fs::create_dir_all(&source_root)
        .await
        .context("create source root dir")?;

    let layout = WorkspaceLayout {
        source_roots: vec![SourceRoot { path: source_root }],
    };

    let config = DistributedRouterConfig {
        listen_addr: ListenAddr::Unix(listen_path),
        worker_command: PathBuf::from("nova-worker"),
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

    let router = QueryRouter::new_distributed(config, layout).await?;
    Ok((env_guard, router))
}

#[tokio::test]
async fn oversized_frame_len_is_rejected_without_killing_accept_loop() -> Result<()> {
    let tmp = TempDir::new()?;
    let listen_path = tmp.path().join("router.sock");
    let (_env_guard, router) = start_router(&tmp, listen_path.clone()).await?;

    // Send an oversized frame header and then stop. The router should reject it without
    // attempting to read the payload (i.e. without stalling on `read_exact`).
    let mut stream = connect_with_retry(&listen_path).await?;
    stream
        .write_u32_le((nova_remote_proto::MAX_MESSAGE_BYTES as u32) + 1)
        .await
        .context("write oversized len")?;
    stream.flush().await.context("flush oversized len")?;

    let close_res = tokio::time::timeout(Duration::from_secs(2), async {
        let mut buf = [0u8; 1];
        stream.read(&mut buf).await
    })
    .await;
    assert!(
        close_res.is_ok(),
        "router did not close connection promptly after oversized frame"
    );

    // Regression test: invalid connections should not terminate the accept loop.
    tokio::time::timeout(Duration::from_secs(2), complete_handshake(&listen_path))
        .await
        .context("timed out waiting for handshake")??;

    router.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn stalled_handshake_does_not_block_other_connections() -> Result<()> {
    let tmp = TempDir::new()?;
    let listen_path = tmp.path().join("router.sock");
    let (_env_guard, router) = start_router(&tmp, listen_path.clone()).await?;

    // First client connects and never sends the initial hello frame.
    let _stalled = connect_with_retry(&listen_path).await?;

    // Second client should still be able to complete the handshake promptly.
    tokio::time::timeout(Duration::from_secs(2), complete_handshake(&listen_path))
        .await
        .context("timed out waiting for handshake")??;

    router.shutdown().await?;
    Ok(())
}
