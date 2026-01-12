#![cfg(feature = "tls")]

use std::collections::HashMap;
use std::path::PathBuf;

use nova_router::{
    tls::TlsServerConfig, DistributedRouterConfig, ListenAddr, QueryRouter, TcpListenAddr,
    TlsClientCertFingerprintAllowlist, WorkspaceLayout,
};

#[tokio::test]
async fn mtls_allowlist_requires_client_ca_verification() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let fingerprint = "00".repeat(32);

    let config = DistributedRouterConfig {
        listen_addr: ListenAddr::Tcp(TcpListenAddr::Tls {
            addr: "127.0.0.1:0".parse().unwrap(),
            config: TlsServerConfig::from_pem_files("unused.pem", "unused.key"),
        }),
        worker_command: PathBuf::from("unused-worker"),
        cache_dir: tmp.path().join("cache"),
        auth_token: None,
        allow_insecure_tcp: false,
        max_rpc_bytes: nova_router::DEFAULT_MAX_RPC_BYTES,
        max_inflight_handshakes: nova_router::DEFAULT_MAX_INFLIGHT_HANDSHAKES,
        max_worker_connections: nova_router::DEFAULT_MAX_WORKER_CONNECTIONS,
        tls_client_cert_fingerprint_allowlist: TlsClientCertFingerprintAllowlist {
            global: vec![fingerprint],
            shards: HashMap::new(),
        },
        spawn_workers: false,
    };

    let err = QueryRouter::new_distributed(
        config,
        WorkspaceLayout {
            source_roots: vec![],
        },
    )
    .await
    .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("client CA"),
        "expected error to mention client CA requirement; got: {msg}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn mtls_allowlist_requires_tcp_tls_transport() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let fingerprint = "00".repeat(32);

    let config = DistributedRouterConfig {
        listen_addr: ListenAddr::Unix(tmp.path().join("router.sock")),
        worker_command: PathBuf::from("unused-worker"),
        cache_dir: tmp.path().join("cache"),
        auth_token: None,
        allow_insecure_tcp: false,
        max_rpc_bytes: nova_router::DEFAULT_MAX_RPC_BYTES,
        max_inflight_handshakes: nova_router::DEFAULT_MAX_INFLIGHT_HANDSHAKES,
        max_worker_connections: nova_router::DEFAULT_MAX_WORKER_CONNECTIONS,
        tls_client_cert_fingerprint_allowlist: TlsClientCertFingerprintAllowlist {
            global: vec![fingerprint],
            shards: HashMap::new(),
        },
        spawn_workers: false,
    };

    let err = QueryRouter::new_distributed(
        config,
        WorkspaceLayout {
            source_roots: vec![],
        },
    )
    .await
    .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("TCP+TLS"),
        "expected error to mention TCP+TLS requirement; got: {msg}"
    );
}
