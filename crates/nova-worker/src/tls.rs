use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;
use std::sync::Once;

use anyhow::{anyhow, Context, Result};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::TlsArgs;

fn ensure_crypto_provider_installed() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

pub async fn connect(
    stream: TcpStream,
    cfg: &Option<TlsArgs>,
) -> Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let cfg = cfg
        .as_ref()
        .ok_or_else(|| anyhow!("--tls-ca-cert is required for tcp+tls"))?;

    let client_config = Arc::new(build_config(cfg)?);
    let connector = TlsConnector::from(client_config);

    let server_name = rustls::pki_types::ServerName::try_from(cfg.domain.as_str())
        .map_err(|_| anyhow!("invalid tls domain {:?}", cfg.domain))?
        .to_owned();

    connector
        .connect(server_name, stream)
        .await
        .context("tls connect")
}

fn build_config(cfg: &TlsArgs) -> Result<rustls::ClientConfig> {
    ensure_crypto_provider_installed();
    let mut roots = rustls::RootCertStore::empty();
    for cert in load_certs(&cfg.ca_cert)? {
        roots.add(cert).context("add root cert")?;
    }

    let builder = rustls::ClientConfig::builder().with_root_certificates(roots);

    match (&cfg.client_cert, &cfg.client_key) {
        (None, None) => Ok(builder.with_no_client_auth()),
        (Some(cert_path), Some(key_path)) => {
            let certs = load_certs(cert_path)?;
            let key = load_private_key(key_path)?;
            builder
                .with_client_auth_cert(certs, key)
                .map_err(|err| anyhow!("invalid TLS client config: {err}"))
        }
        _ => Err(anyhow!(
            "BUG: tls client cert/key should have been validated by argument parsing"
        )),
    }
}

fn load_certs(path: &Path) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>> {
    let file = File::open(path).with_context(|| format!("open cert {path:?}"))?;
    let mut reader = BufReader::new(file);
    let certs = rustls_pemfile::certs(&mut reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("parse cert {path:?}"))?;
    Ok(certs)
}

fn load_private_key(path: &Path) -> Result<rustls::pki_types::PrivateKeyDer<'static>> {
    let file = File::open(path).with_context(|| format!("open key {path:?}"))?;
    let mut reader = BufReader::new(file);

    if let Some(key) = rustls_pemfile::private_key(&mut reader)
        .with_context(|| format!("parse private key {path:?}"))?
    {
        return Ok(key);
    }

    Err(anyhow!("no private key found in {path:?}"))
}
