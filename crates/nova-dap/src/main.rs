use clap::Parser;
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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Best-effort hardening: install structured logging + panic hook early so
    // panics in request handlers are recorded and surfaced to the client.
    let config = nova_config::NovaConfig::default();
    nova_dap::hardening::init(&config, Arc::new(|message| eprintln!("{message}")));

    if cli.legacy {
        nova_dap::server::DapServer::default().run_stdio()
    } else {
        nova_dap::wire_server::run_stdio().await
    }
}
