use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use nova_router::{DistributedRouterConfig, ListenAddr, QueryRouter, TcpListenAddr, WorkspaceLayout};

#[tokio::test]
async fn refuses_plain_tcp_on_non_loopback_by_default() {
    let config = DistributedRouterConfig {
        listen_addr: ListenAddr::Tcp(TcpListenAddr::Plain(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            0,
        ))),
        worker_command: PathBuf::from("nova-worker"),
        cache_dir: std::env::temp_dir(),
        auth_token: None,
        allow_insecure_tcp: false,
        #[cfg(feature = "tls")]
        tls_client_cert_fingerprint_allowlist: Default::default(),
        spawn_workers: false,
    };

    let layout = WorkspaceLayout {
        source_roots: Vec::new(),
    };

    let err = QueryRouter::new_distributed(config, layout)
        .await
        .err()
        .expect("expected router to reject insecure TCP listener");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("plaintext TCP") && msg.contains("not loopback"),
        "unexpected error message: {msg}"
    );
}

#[tokio::test]
async fn refuses_plain_tcp_with_auth_token_by_default() {
    let config = DistributedRouterConfig {
        listen_addr: ListenAddr::Tcp(TcpListenAddr::Plain(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            0,
        ))),
        worker_command: PathBuf::from("nova-worker"),
        cache_dir: std::env::temp_dir(),
        auth_token: Some("secret-token".into()),
        allow_insecure_tcp: false,
        #[cfg(feature = "tls")]
        tls_client_cert_fingerprint_allowlist: Default::default(),
        spawn_workers: false,
    };

    let layout = WorkspaceLayout {
        source_roots: Vec::new(),
    };

    let err = QueryRouter::new_distributed(config, layout)
        .await
        .err()
        .expect("expected router to reject auth token over plaintext TCP");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("plaintext TCP") && msg.contains("auth token"),
        "unexpected error message: {msg}"
    );
}
