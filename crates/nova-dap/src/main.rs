use clap::Parser;
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
    let config = load_config(cli.config);
    nova_dap::hardening::init(&config, Arc::new(|message| eprintln!("{message}")));

    if cli.legacy {
        nova_dap::server::DapServer::default().run_stdio()
    } else {
        nova_dap::wire_server::run_stdio().await
    }
}

fn load_config(cli_path: Option<PathBuf>) -> nova_config::NovaConfig {
    let path = cli_path.or_else(|| std::env::var_os("NOVA_CONFIG").map(PathBuf::from));
    let Some(path) = path else {
        return nova_config::NovaConfig::default();
    };

    match nova_config::NovaConfig::load_from_path(&path) {
        Ok(config) => config,
        Err(err) => {
            eprintln!(
                "nova-dap: failed to load config from {}: {err}; continuing with defaults",
                path.display()
            );
            nova_config::NovaConfig::default()
        }
    }
}
