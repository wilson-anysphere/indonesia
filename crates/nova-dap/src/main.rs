use clap::Parser;

/// Nova Debug Adapter Protocol server (experimental).
///
/// The adapter currently speaks DAP over stdio and provides a minimal subset of
/// requests sufficient for a basic handshake and breakpoint placement.
#[derive(Debug, Parser)]
#[command(name = "nova-dap", version, about)]
struct Cli {}

fn main() -> anyhow::Result<()> {
    let _ = Cli::parse();
    nova_dap::server::DapServer::default().run_stdio()
}

