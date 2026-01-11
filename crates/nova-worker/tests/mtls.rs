#![cfg(feature = "tls")]

use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use nova_remote_proto::v3::{Request, Response};
use nova_remote_rpc::{RouterConfig, RpcConnection};
use nova_router::tls::TlsServerConfig;
use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, SanType,
};
use tempfile::TempDir;
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

        let cfg = RouterConfig {
            worker_id: 1,
            revision: 0,
            ..RouterConfig::default()
        };

        let (conn, welcome) = RpcConnection::handshake_as_router_with_config(accepted.stream, cfg)
            .await
            .map_err(|err| anyhow!("handshake failed: {err}"))?;

        let shard_id = welcome.shard_id;

        let resp = conn
            .call(Request::GetWorkerStats)
            .await
            .map_err(|err| anyhow!("GetWorkerStats failed: {err:?}"))?;

        match resp {
            Response::WorkerStats(ws) => {
                if ws.shard_id != shard_id {
                    return Err(anyhow!(
                        "expected worker stats for shard {shard_id}, got {}",
                        ws.shard_id
                    ));
                }
            }
            other => return Err(anyhow!("expected WorkerStats, got {other:?}")),
        }

        let resp = conn
            .call(Request::Shutdown)
            .await
            .map_err(|err| anyhow!("Shutdown failed: {err:?}"))?;
        match resp {
            Response::Shutdown => {}
            other => return Err(anyhow!("expected Shutdown response, got {other:?}")),
        }

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

    timeout(Duration::from_secs(10), accept_task)
        .await
        .context("wait for accept task")?
        .context("join accept task")??;

    let status = timeout(Duration::from_secs(10), child.wait())
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

struct GeneratedCert {
    cert: Certificate,
    key: KeyPair,
}

fn generate_test_pki(dir: &Path) -> Result<TestPkiPaths> {
    let ca = generate_ca()?;

    let server = generate_leaf_cert(
        "localhost",
        vec!["localhost".to_string()],
        vec![SanType::IpAddress(IpAddr::V4(Ipv4Addr::LOCALHOST))],
        ExtendedKeyUsagePurpose::ServerAuth,
        &ca,
    )?;

    let client = generate_leaf_cert(
        "nova-worker",
        Vec::new(),
        Vec::new(),
        ExtendedKeyUsagePurpose::ClientAuth,
        &ca,
    )?;

    let ca_cert = dir.join("ca.pem");
    let server_cert = dir.join("server.pem");
    let server_key = dir.join("server.key");
    let client_cert = dir.join("client.pem");
    let client_key = dir.join("client.key");

    std::fs::write(&ca_cert, ca.cert.pem()).context("write ca cert")?;
    std::fs::write(&server_cert, server.cert.pem()).context("write server cert")?;
    std::fs::write(&server_key, server.key.serialize_pem()).context("write server key")?;
    std::fs::write(&client_cert, client.cert.pem()).context("write client cert")?;
    std::fs::write(&client_key, client.key.serialize_pem()).context("write client key")?;

    Ok(TestPkiPaths {
        ca_cert,
        server_cert,
        server_key,
        client_cert,
        client_key,
    })
}

fn generate_ca() -> Result<GeneratedCert> {
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params
        .distinguished_name
        .push(DnType::CommonName, "nova-test-ca");

    let key = KeyPair::generate().context("generate CA key")?;
    let cert = params.self_signed(&key).context("self-sign CA cert")?;
    Ok(GeneratedCert { cert, key })
}

fn generate_leaf_cert(
    common_name: &str,
    dns_names: Vec<String>,
    mut extra_sans: Vec<SanType>,
    eku: ExtendedKeyUsagePurpose,
    ca: &GeneratedCert,
) -> Result<GeneratedCert> {
    let mut params = CertificateParams::new(dns_names).context("create certificate params")?;
    params.is_ca = IsCa::NoCa;
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    params.extended_key_usages = vec![eku];
    params
        .distinguished_name
        .push(DnType::CommonName, common_name);

    params.subject_alt_names.append(&mut extra_sans);

    let key = KeyPair::generate().context("generate leaf key")?;
    let cert = params
        .signed_by(&key, &ca.cert, &ca.key)
        .context("sign leaf cert")?;
    Ok(GeneratedCert { cert, key })
}
