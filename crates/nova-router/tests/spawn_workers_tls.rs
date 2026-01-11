#![cfg(feature = "tls")]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use nova_router::{
    tls::TlsServerConfig, DistributedRouterConfig, ListenAddr, QueryRouter, TcpListenAddr,
    WorkspaceLayout,
};
use rcgen::{CertificateParams, ExtendedKeyUsagePurpose, KeyPair};

#[tokio::test]
async fn spawn_workers_with_tcp_tls_is_rejected() {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path();

    let server_cert = {
        let mut params = CertificateParams::new(vec!["localhost".into()]).unwrap();
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];

        let key = KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        (cert, key)
    };
    let cert_pem = server_cert.0.pem();
    let key_pem = server_cert.1.serialize_pem();

    let cert_path = dir.join("router.pem");
    let key_path = dir.join("router.key");
    std::fs::write(&cert_path, cert_pem).unwrap();
    std::fs::write(&key_path, key_pem).unwrap();

    let config = DistributedRouterConfig {
        listen_addr: ListenAddr::Tcp(TcpListenAddr::Tls {
            addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            config: TlsServerConfig::from_pem_files(&cert_path, &key_path),
        }),
        // The worker command is never reached because validation rejects the configuration.
        worker_command: PathBuf::from("nova-worker"),
        cache_dir: dir.join("cache"),
        auth_token: None,
        allow_insecure_tcp: false,
        max_rpc_bytes: nova_router::DEFAULT_MAX_RPC_BYTES,
        max_inflight_handshakes: nova_router::DEFAULT_MAX_INFLIGHT_HANDSHAKES,
        max_worker_connections: nova_router::DEFAULT_MAX_WORKER_CONNECTIONS,
        tls_client_cert_fingerprint_allowlist: Default::default(),
        spawn_workers: true,
    };

    let err = QueryRouter::new_distributed(
        config,
        WorkspaceLayout {
            source_roots: Vec::new(),
        },
    )
    .await
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("spawn_workers") && msg.contains("tcp+tls"),
        "unexpected error message: {msg}"
    );
}
