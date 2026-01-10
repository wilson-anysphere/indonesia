use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::TlsArgs;

pub async fn connect(
    stream: TcpStream,
    cfg: &Option<TlsArgs>,
) -> Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let cfg = cfg
        .as_ref()
        .ok_or_else(|| anyhow!("--tls-ca-cert is required for tcp+tls"))?;

    let client_config = Arc::new(build_config(&cfg.ca_cert)?);
    let connector = TlsConnector::from(client_config);

    let server_name = rustls::pki_types::ServerName::try_from(cfg.domain.as_str())
        .map_err(|_| anyhow!("invalid tls domain {:?}", cfg.domain))?
        .to_owned();

    connector
        .connect(server_name, stream)
        .await
        .context("tls connect")
}

fn build_config(ca_cert: &Path) -> Result<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    for cert in load_certs(ca_cert)? {
        roots.add(cert).context("add root cert")?;
    }

    Ok(rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth())
}

fn load_certs(path: &Path) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>> {
    let file = File::open(path).with_context(|| format!("open ca cert {path:?}"))?;
    let mut reader = BufReader::new(file);
    let certs = rustls_pemfile::certs(&mut reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("parse ca cert {path:?}"))?;
    Ok(certs)
}
