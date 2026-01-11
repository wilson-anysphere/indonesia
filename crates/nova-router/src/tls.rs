use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use sha2::Digest;
use tokio::net::TcpStream;
use tokio_rustls::TlsAcceptor;

#[derive(Clone, Debug)]
pub struct TlsServerConfig {
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
    /// When set, clients must present a certificate signed by this CA (mTLS).
    pub client_ca_cert_path: Option<PathBuf>,
}

impl TlsServerConfig {
    pub fn from_pem_files(cert_path: impl Into<PathBuf>, key_path: impl Into<PathBuf>) -> Self {
        Self {
            cert_path: cert_path.into(),
            key_path: key_path.into(),
            client_ca_cert_path: None,
        }
    }

    pub fn with_client_ca_cert(mut self, client_ca_cert_path: impl Into<PathBuf>) -> Self {
        self.client_ca_cert_path = Some(client_ca_cert_path.into());
        self
    }

    fn build(&self) -> Result<Arc<rustls::ServerConfig>> {
        let certs = load_certs(&self.cert_path)?;
        let key = load_private_key(&self.key_path)?;

        let builder = rustls::ServerConfig::builder();
        let builder = if let Some(ca_cert_path) = &self.client_ca_cert_path {
            let roots = load_root_store(ca_cert_path)?;
            let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
                .build()
                .map_err(|err| anyhow!("invalid TLS client verifier: {err}"))?;
            builder.with_client_cert_verifier(verifier)
        } else {
            builder.with_no_client_auth()
        };

        let config = builder
            .with_single_cert(certs, key)
            .map_err(|err| anyhow!("invalid TLS config: {err}"))?;
        Ok(Arc::new(config))
    }
}

#[derive(Debug)]
pub struct AcceptedTlsStream {
    pub stream: tokio_rustls::server::TlsStream<TcpStream>,
    /// SHA-256 fingerprint of the presented leaf client certificate, if any.
    pub client_cert_fingerprint: Option<String>,
}

pub async fn accept(stream: TcpStream, cfg: TlsServerConfig) -> Result<AcceptedTlsStream> {
    let acceptor = TlsAcceptor::from(cfg.build()?);
    let stream = acceptor.accept(stream).await.context("tls accept")?;
    let client_cert_fingerprint = tls_client_cert_fingerprint(&stream);
    Ok(AcceptedTlsStream {
        stream,
        client_cert_fingerprint,
    })
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

fn load_root_store(path: &Path) -> Result<rustls::RootCertStore> {
    let certs = load_certs(path)?;
    let mut roots = rustls::RootCertStore::empty();
    for cert in certs {
        roots.add(cert).context("add root cert")?;
    }
    Ok(roots)
}

fn tls_client_cert_fingerprint(
    stream: &tokio_rustls::server::TlsStream<TcpStream>,
) -> Option<String> {
    let (_, conn) = stream.get_ref();
    let certs = conn.peer_certificates()?;
    let leaf = certs.first()?;
    let mut hasher = sha2::Sha256::new();
    hasher.update(leaf.as_ref());
    Some(hex::encode(hasher.finalize()))
}
