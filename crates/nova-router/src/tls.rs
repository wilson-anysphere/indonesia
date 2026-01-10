use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use tokio::net::TcpStream;
use tokio_rustls::TlsAcceptor;

#[derive(Clone, Debug)]
pub struct TlsServerConfig {
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
}

impl TlsServerConfig {
    pub fn from_pem_files(cert_path: impl Into<PathBuf>, key_path: impl Into<PathBuf>) -> Self {
        Self {
            cert_path: cert_path.into(),
            key_path: key_path.into(),
        }
    }

    fn build(&self) -> Result<Arc<rustls::ServerConfig>> {
        let certs = load_certs(&self.cert_path)?;
        let key = load_private_key(&self.key_path)?;

        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(|err| anyhow!("invalid TLS config: {err}"))?;
        Ok(Arc::new(config))
    }
}

pub async fn accept(
    stream: TcpStream,
    cfg: TlsServerConfig,
) -> Result<tokio_rustls::server::TlsStream<TcpStream>> {
    let acceptor = TlsAcceptor::from(cfg.build()?);
    acceptor.accept(stream).await.context("tls accept")
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
