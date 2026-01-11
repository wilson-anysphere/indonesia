#![cfg(feature = "tls")]

use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use nova_remote_proto::RpcMessage;
use nova_router::tls::TlsServerConfig;
use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyUsagePurpose, SanType,
};
use tempfile::TempDir;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::time::timeout;

#[tokio::test]
async fn router_mtls_rejects_worker_without_client_cert() -> Result<()> {
    let tmp = TempDir::new()?;
    let pki = generate_test_pki(tmp.path())?;

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;

    let mut server_cfg =
        TlsServerConfig::from_pem_files(pki.server_cert.clone(), pki.server_key.clone());
    server_cfg.client_ca_path = Some(pki.ca_cert.clone());
    server_cfg.require_client_auth = true;

    let accept_task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.context("accept tcp")?;
        nova_router::tls::accept(stream, server_cfg).await?;
        Result::<()>::Err(anyhow!(
            "expected TLS handshake to fail without a client certificate"
        ))
    });

    let worker_bin = PathBuf::from(env!("CARGO_BIN_EXE_nova-worker"));
    let mut child = Command::new(worker_bin)
        .arg("--connect")
        .arg(format!("tcp+tls:{addr}"))
        .arg("--tls-ca-cert")
        .arg(&pki.ca_cert)
        .arg("--tls-domain")
        .arg("localhost")
        .arg("--shard-id")
        .arg("0")
        .arg("--cache-dir")
        .arg(tmp.path().join("cache"))
        .spawn()
        .context("spawn nova-worker")?;

    let status = timeout(Duration::from_secs(5), child.wait())
        .await
        .context("wait for worker exit")?
        .context("join worker")?;
    assert!(
        !status.success(),
        "expected worker to fail TLS handshake without a client cert"
    );

    // Server should observe the handshake failure.
    let accept_res = timeout(Duration::from_secs(5), accept_task)
        .await
        .context("wait for accept task")?
        .context("join accept task")?;
    assert!(accept_res.is_err(), "expected TLS accept to fail");

    Ok(())
}

#[tokio::test]
async fn router_mtls_accepts_worker_with_valid_client_cert() -> Result<()> {
    let tmp = TempDir::new()?;
    let pki = generate_test_pki(tmp.path())?;

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;

    let mut server_cfg =
        TlsServerConfig::from_pem_files(pki.server_cert.clone(), pki.server_key.clone());
    server_cfg.client_ca_path = Some(pki.ca_cert.clone());
    server_cfg.require_client_auth = true;

    let accept_task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.context("accept tcp")?;
        let accepted = nova_router::tls::accept(stream, server_cfg).await?;
        let mut stream = accepted.stream;

        let hello = read_message(&mut stream).await?;
        let shard_id = match hello {
            RpcMessage::WorkerHello { shard_id, .. } => shard_id,
            other => return Err(anyhow!("expected WorkerHello, got {other:?}")),
        };

        write_message(
            &mut stream,
            &RpcMessage::RouterHello {
                worker_id: 1,
                shard_id,
                revision: 0,
                protocol_version: nova_remote_proto::PROTOCOL_VERSION,
            },
        )
        .await?;

        write_message(&mut stream, &RpcMessage::GetWorkerStats).await?;
        let stats = read_message(&mut stream).await?;
        match stats {
            RpcMessage::WorkerStats(ws) => {
                if ws.shard_id != shard_id {
                    return Err(anyhow!(
                        "expected worker stats for shard {shard_id}, got {}",
                        ws.shard_id
                    ));
                }
            }
            other => return Err(anyhow!("expected WorkerStats, got {other:?}")),
        }

        write_message(&mut stream, &RpcMessage::Shutdown).await?;
        Ok::<_, anyhow::Error>(())
    });

    let worker_bin = PathBuf::from(env!("CARGO_BIN_EXE_nova-worker"));
    let mut child = Command::new(worker_bin)
        .arg("--connect")
        .arg(format!("tcp+tls:{addr}"))
        .arg("--tls-ca-cert")
        .arg(&pki.ca_cert)
        .arg("--tls-domain")
        .arg("localhost")
        .arg("--tls-client-cert")
        .arg(&pki.client_cert)
        .arg("--tls-client-key")
        .arg(&pki.client_key)
        .arg("--shard-id")
        .arg("0")
        .arg("--cache-dir")
        .arg(tmp.path().join("cache"))
        .spawn()
        .context("spawn nova-worker")?;

    timeout(Duration::from_secs(5), accept_task)
        .await
        .context("wait for accept task")?
        .context("join accept task")?;

    let status = timeout(Duration::from_secs(5), child.wait())
        .await
        .context("wait for worker exit")?
        .context("join worker")?;
    assert!(status.success(), "expected worker to exit cleanly");

    Ok(())
}

struct TestPkiPaths {
    ca_cert: PathBuf,
    server_cert: PathBuf,
    server_key: PathBuf,
    client_cert: PathBuf,
    client_key: PathBuf,
}

fn generate_test_pki(dir: &Path) -> Result<TestPkiPaths> {
    let ca = generate_ca()?;

    let server = generate_leaf_cert(
        "localhost",
        &[
            SanType::DnsName("localhost".into()),
            SanType::IpAddress(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        ],
        ExtendedKeyUsagePurpose::ServerAuth,
        &ca,
    )?;

    let client = generate_leaf_cert("nova-worker", &[], ExtendedKeyUsagePurpose::ClientAuth, &ca)?;

    let ca_cert = dir.join("ca.pem");
    let server_cert = dir.join("server.pem");
    let server_key = dir.join("server.key");
    let client_cert = dir.join("client.pem");
    let client_key = dir.join("client.key");

    std::fs::write(&ca_cert, ca.serialize_pem()?).context("write ca cert")?;
    std::fs::write(&server_cert, server.serialize_pem_with_signer(&ca)?)
        .context("write server cert")?;
    std::fs::write(&server_key, server.serialize_private_key_pem()).context("write server key")?;
    std::fs::write(&client_cert, client.serialize_pem_with_signer(&ca)?)
        .context("write client cert")?;
    std::fs::write(&client_key, client.serialize_private_key_pem()).context("write client key")?;

    Ok(TestPkiPaths {
        ca_cert,
        server_cert,
        server_key,
        client_cert,
        client_key,
    })
}

fn generate_ca() -> Result<Certificate> {
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params
        .distinguished_name
        .push(DnType::CommonName, "nova-test-ca");
    Certificate::from_params(params).context("generate CA cert")
}

fn generate_leaf_cert(
    common_name: &str,
    subject_alt_names: &[SanType],
    eku: ExtendedKeyUsagePurpose,
    ca: &Certificate,
) -> Result<Certificate> {
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::NoCa;
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    params.extended_key_usages = vec![eku];
    params
        .distinguished_name
        .push(DnType::CommonName, common_name);
    params.subject_alt_names = subject_alt_names.to_vec();
    Certificate::from_params(params)
        .context("generate leaf cert")
        .and_then(|cert| {
            // Ensure it can be signed by the CA (serialization happens when writing files).
            cert.serialize_der_with_signer(ca)
                .context("sign leaf cert")?;
            Ok(cert)
        })
}

async fn write_message(stream: &mut (impl AsyncWrite + Unpin), message: &RpcMessage) -> Result<()> {
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

async fn read_message(stream: &mut (impl AsyncRead + Unpin)) -> Result<RpcMessage> {
    let len = stream.read_u32_le().await.context("read message len")?;
    let mut buf = vec![0u8; len as usize];
    stream
        .read_exact(&mut buf)
        .await
        .context("read message payload")?;
    nova_remote_proto::decode_message(&buf)
}
