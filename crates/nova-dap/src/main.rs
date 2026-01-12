use clap::Parser;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

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
    listen: Option<SocketAddr>,

    /// Path to a TOML config file.
    ///
    /// If unset, `NOVA_CONFIG` is used as a fallback. When neither are provided
    /// the adapter uses in-memory defaults.
    #[arg(long)]
    config: Option<PathBuf>,
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

async fn run_tcp(addr: SocketAddr) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    eprintln!("listening on {bound}");

    let (stream, _) = listener.accept().await?;
    // Accept exactly one connection: stop listening once a client has connected.
    drop(listener);
    stream.set_nodelay(true).ok();
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
