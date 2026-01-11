#![cfg(feature = "tls")]

use std::collections::HashMap;
use std::io::{BufReader, Cursor};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Context};
use nova_remote_proto::{RpcMessage, ShardId};
use nova_router::{
    tls::TlsServerConfig, DistributedRouterConfig, ListenAddr, QueryRouter, SourceRoot,
    TcpListenAddr, TlsClientCertFingerprintAllowlist, WorkspaceLayout,
};
use sha2::Digest;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

fn sha256_fingerprint_hex(der: &[u8]) -> String {
    let mut hasher = sha2::Sha256::new();
    hasher.update(der);
    hex::encode(hasher.finalize())
}

fn load_certs(pem: &[u8]) -> anyhow::Result<Vec<rustls::pki_types::CertificateDer<'static>>> {
    let mut reader = BufReader::new(Cursor::new(pem));
    Ok(rustls_pemfile::certs(&mut reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("parse cert")?)
}

fn load_private_key(pem: &[u8]) -> anyhow::Result<rustls::pki_types::PrivateKeyDer<'static>> {
    let mut reader = BufReader::new(Cursor::new(pem));
    rustls_pemfile::private_key(&mut reader)
        .context("parse private key")?
        .ok_or_else(|| anyhow!("no private key found"))
}

async fn connect_mtls(
    addr: SocketAddr,
    domain: &str,
    ca_pem: &[u8],
    client_cert_pem: &[u8],
    client_key_pem: &[u8],
) -> anyhow::Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let mut roots = rustls::RootCertStore::empty();
    for cert in load_certs(ca_pem)? {
        roots.add(cert).context("add root cert")?;
    }

    let client_certs = load_certs(client_cert_pem)?;
    let client_key = load_private_key(client_key_pem)?;

    let client_config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(client_certs, client_key)
        .map_err(|err| anyhow!("invalid TLS client config: {err}"))?;

    let connector = TlsConnector::from(Arc::new(client_config));
    let server_name = rustls::pki_types::ServerName::try_from(domain)
        .map_err(|_| anyhow!("invalid tls domain {:?}", domain))?
        .to_owned();

    let tcp = TcpStream::connect(addr).await.context("connect tcp")?;
    connector
        .connect(server_name, tcp)
        .await
        .context("tls connect")
}

async fn write_message(
    stream: &mut (impl AsyncWrite + Unpin),
    message: &RpcMessage,
) -> anyhow::Result<()> {
    let payload = nova_remote_proto::encode_message(message)?;
    let len: u32 = payload
        .len()
        .try_into()
        .map_err(|_| anyhow!("message too large"))?;

    stream
        .write_u32_le(len)
        .await
        .context("write message len")?;
    stream
        .write_all(&payload)
        .await
        .context("write message payload")?;
    stream.flush().await.context("flush message")?;
    Ok(())
}

async fn read_message(stream: &mut (impl AsyncRead + Unpin)) -> anyhow::Result<RpcMessage> {
    let len = stream.read_u32_le().await.context("read message len")?;
    let mut buf = vec![0u8; len as usize];
    stream
        .read_exact(&mut buf)
        .await
        .context("read message payload")?;
    Ok(nova_remote_proto::decode_message(&buf)?)
}

async fn connect_with_retry<F, Fut, T>(mut f: F) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match f().await {
            Ok(v) => return Ok(v),
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                continue;
            }
            Err(err) => return Err(err),
        }
    }
}

fn pick_unused_tcp_addr() -> anyhow::Result<SocketAddr> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").context("bind ephemeral port")?;
    let addr = listener.local_addr().context("read local_addr")?;
    drop(listener);
    Ok(addr)
}

#[tokio::test]
async fn mtls_shard_allowlist_scopes_workers_by_cert_fingerprint() -> anyhow::Result<()> {
    let tmp = tempfile::TempDir::new()?;
    let dir = tmp.path();

    let ca = {
        let mut params = rcgen::CertificateParams::default();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params.key_usages = vec![
            rcgen::KeyUsagePurpose::KeyCertSign,
            rcgen::KeyUsagePurpose::DigitalSignature,
            rcgen::KeyUsagePurpose::CrlSign,
        ];
        rcgen::Certificate::from_params(params)?
    };
    let ca_pem = ca.serialize_pem()?;

    let server = {
        let mut params = rcgen::CertificateParams::new(vec!["localhost".into()]);
        params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        rcgen::Certificate::from_params(params)?
    };
    let server_cert_pem = server.serialize_pem_with_signer(&ca)?;
    let server_key_pem = server.serialize_private_key_pem();

    let client_a = rcgen::Certificate::from_params({
        let mut params = rcgen::CertificateParams::default();
        params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ClientAuth];
        params
    })?;
    let client_a_cert_der = client_a.serialize_der_with_signer(&ca)?;
    let client_a_cert_pem = client_a.serialize_pem_with_signer(&ca)?;
    let client_a_key_pem = client_a.serialize_private_key_pem();
    let client_a_fp = sha256_fingerprint_hex(&client_a_cert_der);

    let client_b = rcgen::Certificate::from_params({
        let mut params = rcgen::CertificateParams::default();
        params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ClientAuth];
        params
    })?;
    let client_b_cert_der = client_b.serialize_der_with_signer(&ca)?;
    let client_b_cert_pem = client_b.serialize_pem_with_signer(&ca)?;
    let client_b_key_pem = client_b.serialize_private_key_pem();
    let client_b_fp = sha256_fingerprint_hex(&client_b_cert_der);

    let ca_path = dir.join("ca.pem");
    let server_cert_path = dir.join("router.pem");
    let server_key_path = dir.join("router.key");
    tokio::fs::write(&ca_path, &ca_pem).await?;
    tokio::fs::write(&server_cert_path, &server_cert_pem).await?;
    tokio::fs::write(&server_key_path, &server_key_pem).await?;

    let addr = pick_unused_tcp_addr()?;
    let mut shards = HashMap::new();
    shards.insert(0 as ShardId, vec![client_a_fp.clone()]);
    shards.insert(1 as ShardId, vec![client_b_fp.clone()]);

    let config = DistributedRouterConfig {
        listen_addr: ListenAddr::Tcp(TcpListenAddr::Tls {
            addr,
            config: TlsServerConfig::from_pem_files(&server_cert_path, &server_key_path)
                .with_client_ca_cert(&ca_path),
        }),
        worker_command: PathBuf::from("unused-worker-bin"),
        cache_dir: dir.join("cache"),
        auth_token: None,
        tls_client_cert_fingerprint_allowlist: TlsClientCertFingerprintAllowlist {
            global: Vec::new(),
            shards,
        },
        spawn_workers: false,
    };

    let layout = WorkspaceLayout {
        source_roots: vec![
            SourceRoot {
                path: dir.join("shard0"),
            },
            SourceRoot {
                path: dir.join("shard1"),
            },
        ],
    };
    tokio::fs::create_dir_all(&layout.source_roots[0].path).await?;
    tokio::fs::create_dir_all(&layout.source_roots[1].path).await?;

    let router = QueryRouter::new_distributed(config, layout).await?;

    // Client A can connect as shard 0.
    let mut stream_a0 = connect_with_retry(|| {
        let ca_pem = ca_pem.clone();
        let cert_pem = client_a_cert_pem.clone();
        let key_pem = client_a_key_pem.clone();
        async move {
            connect_mtls(
                addr,
                "localhost",
                ca_pem.as_bytes(),
                cert_pem.as_bytes(),
                key_pem.as_bytes(),
            )
            .await
        }
    })
    .await?;

    write_message(
        &mut stream_a0,
        &RpcMessage::WorkerHello {
            shard_id: 0,
            auth_token: None,
            cached_index: None,
        },
    )
    .await?;
    let resp = read_message(&mut stream_a0).await?;
    match resp {
        RpcMessage::RouterHello { shard_id, .. } => assert_eq!(shard_id, 0),
        other => return Err(anyhow!("expected RouterHello, got {other:?}")),
    }

    // Client A is rejected for shard 1.
    let mut stream_a1 = connect_with_retry(|| {
        let ca_pem = ca_pem.clone();
        let cert_pem = client_a_cert_pem.clone();
        let key_pem = client_a_key_pem.clone();
        async move {
            connect_mtls(
                addr,
                "localhost",
                ca_pem.as_bytes(),
                cert_pem.as_bytes(),
                key_pem.as_bytes(),
            )
            .await
        }
    })
    .await?;
    write_message(
        &mut stream_a1,
        &RpcMessage::WorkerHello {
            shard_id: 1,
            auth_token: None,
            cached_index: None,
        },
    )
    .await?;
    let resp = read_message(&mut stream_a1).await?;
    assert!(matches!(resp, RpcMessage::Error { .. }));

    // Client B can connect as shard 1.
    let mut stream_b1 = connect_with_retry(|| {
        let ca_pem = ca_pem.clone();
        let cert_pem = client_b_cert_pem.clone();
        let key_pem = client_b_key_pem.clone();
        async move {
            connect_mtls(
                addr,
                "localhost",
                ca_pem.as_bytes(),
                cert_pem.as_bytes(),
                key_pem.as_bytes(),
            )
            .await
        }
    })
    .await?;
    write_message(
        &mut stream_b1,
        &RpcMessage::WorkerHello {
            shard_id: 1,
            auth_token: None,
            cached_index: None,
        },
    )
    .await?;
    let resp = read_message(&mut stream_b1).await?;
    match resp {
        RpcMessage::RouterHello { shard_id, .. } => assert_eq!(shard_id, 1),
        other => return Err(anyhow!("expected RouterHello, got {other:?}")),
    }

    router.shutdown().await?;
    Ok(())
}
