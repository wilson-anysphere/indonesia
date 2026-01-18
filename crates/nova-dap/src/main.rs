use clap::Parser;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

/// Nova Debug Adapter Protocol server (experimental).
///
/// The adapter currently speaks DAP over stdio and provides a minimal subset of
/// requests sufficient for a basic handshake and breakpoint placement.
#[derive(Debug, Parser)]
#[command(name = "nova-dap", version, about)]
struct Cli {
    /// Run the legacy (mock/skeleton) DAP server implementation.
    ///
    /// The default implementation uses the wire-level JDWP client and can attach
    /// to a real JVM.
    #[arg(long)]
    legacy: bool,

    /// Bind a TCP listener and serve DAP over a single incoming connection.
    ///
    /// When omitted, the adapter speaks DAP over stdio (the default).
    ///
    /// Note: `--listen` is only supported for the default (wire-level) adapter.
    /// `--legacy --listen` is rejected.
    #[arg(long)]
    listen: Option<ListenAddr>,

    /// Path to a TOML config file.
    ///
    /// If unset, `NOVA_CONFIG` is used as a fallback. When neither are provided
    /// the adapter uses in-memory defaults.
    #[arg(long)]
    config: Option<PathBuf>,
}

/// Socket address argument for `--listen`.
///
/// We accept:
/// - `127.0.0.1:4711` (IPv4)
/// - `[::1]:4711` (IPv6)
/// - `localhost:4711` (hostname)
///
/// We intentionally reject `:4711` to avoid accidentally binding to a public
/// interface due to an omitted host.
#[derive(Debug, Clone)]
struct ListenAddr(String);

impl ListenAddr {
    fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::str::FromStr for ListenAddr {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.is_empty() {
            return Err("expected host:port (e.g. 127.0.0.1:0)".to_string());
        }

        // Fast path: accept standard socket address formats (IPv4 and bracketed IPv6).
        if s.parse::<SocketAddr>().is_ok() {
            return Ok(Self(s.to_string()));
        }

        if s.starts_with(':') {
            return Err("expected host:port (e.g. 127.0.0.1:0), not just :port".to_string());
        }

        // Hostname support: validate a basic `host:port` shape.
        let Some((host, port)) = s.rsplit_once(':') else {
            return Err("expected host:port (e.g. 127.0.0.1:0)".to_string());
        };
        if host.is_empty() || port.is_empty() {
            return Err("expected host:port (e.g. 127.0.0.1:0)".to_string());
        }
        if host.contains(['[', ']', ':']) {
            return Err("invalid listen address; use `[::1]:4711` for IPv6".to_string());
        }
        port.parse::<u16>()
            .map_err(|_| format!("invalid port {port:?} in listen address"))?;

        Ok(Self(s.to_string()))
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Best-effort hardening: install structured logging + panic hook early so
    // panics in request handlers are recorded and surfaced to the client.
    let (mut config, config_warning) = load_config(cli.config);
    if cli.listen.is_some() {
        // In TCP mode, stdout/stderr are reserved for human/tooling output.
        // Keep the process quiet so clients can safely parse the port discovery
        // line from stderr.
        config.logging.stderr = false;
    }
    nova_dap::hardening::init(&config, Arc::new(|message| eprintln!("{message}")));
    if let Some(warning) = config_warning {
        tracing::warn!(target: "nova.dap", "{warning}");
    }

    match (cli.legacy, cli.listen) {
        (true, Some(_)) => {
            anyhow::bail!("nova-dap: --listen is not supported with --legacy")
        }
        (true, None) => nova_dap::server::DapServer::default().run_stdio(),
        (false, None) => nova_dap::wire_server::run_stdio().await,
        (false, Some(addr)) => run_tcp(addr).await,
    }
}

async fn run_tcp(addr: ListenAddr) -> anyhow::Result<()> {
    // Prefer IPv4 addresses (e.g. for `localhost`) when multiple results are returned.
    // This keeps the output stable (`127.0.0.1:<port>`) and avoids surprises for
    // clients that don't expect bracketed IPv6 socket addresses.
    let mut addrs = tokio::net::lookup_host(addr.as_str())
        .await?
        .collect::<Vec<_>>();
    addrs.sort_by_key(|addr| match addr {
        SocketAddr::V4(_) => 0,
        SocketAddr::V6(_) => 1,
    });

    let mut last_error = None;
    let mut listener = None;
    for addr in addrs {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(bound) => {
                listener = Some(bound);
                break;
            }
            Err(err) => last_error = Some(err),
        }
    }

    let Some(listener) = listener else {
        return Err(anyhow::Error::from(last_error.unwrap_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::AddrNotAvailable, "no addresses to bind")
        })));
    };

    let bound = listener.local_addr()?;
    eprintln!("listening on {bound}");

    let (stream, _) = listener.accept().await?;
    // Accept exactly one connection: stop listening once a client has connected.
    drop(listener);
    static TCP_NODELAY_ERROR_LOGGED: OnceLock<()> = OnceLock::new();
    if let Err(err) = stream.set_nodelay(true) {
        if TCP_NODELAY_ERROR_LOGGED.set(()).is_ok() {
            tracing::debug!(
                target = "nova.dap",
                error = %err,
                "failed to enable TCP_NODELAY (best effort)"
            );
        }
    }
    let (reader, writer) = stream.into_split();

    nova_dap::wire_server::run(reader, writer)
        .await
        .map_err(anyhow::Error::from)
}

fn load_config(cli_path: Option<PathBuf>) -> (nova_config::NovaConfig, Option<String>) {
    let path = cli_path.or_else(|| std::env::var_os("NOVA_CONFIG").map(PathBuf::from));
    let Some(path) = path else {
        return (nova_config::NovaConfig::default(), None);
    };

    match nova_config::NovaConfig::load_from_path(&path) {
        Ok(config) => (config, None),
        Err(err) => (
            nova_config::NovaConfig::default(),
            Some(format!(
                "nova-dap: failed to load config from {}: {err}; continuing with defaults",
                path.display()
            )),
        ),
    }
}
