#[cfg(unix)]
mod unix {
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::time::Duration;

    use nova_router::{DistributedRouterConfig, ListenAddr, QueryRouter, WorkspaceLayout};

    #[tokio::test]
    async fn unix_socket_has_restrictive_permissions() -> anyhow::Result<()> {
        let tmp = tempfile::TempDir::new()?;
        let socket_dir = tmp.path().join("ipc").join("router");
        let socket_path = socket_dir.join("router.sock");

        let config = DistributedRouterConfig {
            listen_addr: ListenAddr::Unix(socket_path.clone()),
            // Not used because `spawn_workers` is false.
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

        let router = QueryRouter::new_distributed(
            config,
            WorkspaceLayout {
                source_roots: vec![],
            },
        )
        .await?;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            if let Ok(meta) = std::fs::metadata(&socket_path) {
                let mode = meta.permissions().mode() & 0o777;
                assert_eq!(mode & 0o002, 0, "unix socket is world-writable: {mode:03o}");
                assert_eq!(mode & 0o020, 0, "unix socket is group-writable: {mode:03o}");
                break;
            }

            if tokio::time::Instant::now() >= deadline {
                anyhow::bail!("timed out waiting for unix socket {socket_path:?} to be created");
            }

            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let dir_mode = std::fs::metadata(&socket_dir)?.permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "socket dir is not 0700: {dir_mode:03o}");

        router.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn unix_socket_dir_permissions_are_hardened_when_dir_preexists() -> anyhow::Result<()> {
        let tmp = tempfile::TempDir::new()?;
        let socket_dir = tmp.path().join("ipc").join("router");
        std::fs::create_dir_all(&socket_dir)?;

        // Simulate a wrapper/launcher creating the directory with a permissive default.
        std::fs::set_permissions(&socket_dir, std::fs::Permissions::from_mode(0o755))?;
        let pre_mode = std::fs::metadata(&socket_dir)?.permissions().mode() & 0o777;
        assert_eq!(
            pre_mode, 0o755,
            "expected pre-created socket dir to be 0755: {pre_mode:03o}"
        );

        let socket_path = socket_dir.join("router.sock");

        let config = DistributedRouterConfig {
            listen_addr: ListenAddr::Unix(socket_path.clone()),
            // Not used because `spawn_workers` is false.
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

        let router = QueryRouter::new_distributed(
            config,
            WorkspaceLayout {
                source_roots: vec![],
            },
        )
        .await?;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            if std::fs::metadata(&socket_path).is_ok() {
                break;
            }

            if tokio::time::Instant::now() >= deadline {
                anyhow::bail!("timed out waiting for unix socket {socket_path:?} to be created");
            }

            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let dir_mode = std::fs::metadata(&socket_dir)?.permissions().mode() & 0o777;
        assert_eq!(
            dir_mode, 0o700,
            "pre-existing socket dir was not corrected to 0700: {dir_mode:03o}"
        );

        router.shutdown().await?;
        Ok(())
    }
}
