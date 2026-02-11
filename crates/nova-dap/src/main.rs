use clap::Parser;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
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
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{}", sanitize_anyhow_error_message(&err));
            ExitCode::FAILURE
        }
    }
}

async fn run() -> anyhow::Result<()> {
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

fn sanitize_anyhow_error_message(err: &anyhow::Error) -> String {
    // `serde_json::Error` display strings can include user-provided scalar values (e.g.
    // `invalid type: string "..."`). Avoid echoing those values to stderr if DAP framing or
    // request parsing fails.
    if err.chain().any(contains_serde_json_error) {
        sanitize_json_error_message(&format!("{err:#}"))
    } else {
        format!("{err:#}")
    }
}

fn contains_serde_json_error(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(err) = current {
        if err.is::<serde_json::Error>() {
            return true;
        }

        if let Some(build_err) = err.downcast_ref::<nova_build::BuildError>() {
            match build_err {
                nova_build::BuildError::Io(io_err) => {
                    if contains_serde_json_error(io_err) {
                        return true;
                    }
                }
                nova_build::BuildError::Cache(cache_err) => {
                    if contains_serde_json_error(cache_err) {
                        return true;
                    }
                }
                _ => {}
            }
        }

        if let Some(io_err) = err.downcast_ref::<std::io::Error>() {
            if let Some(inner) = io_err.get_ref() {
                let inner: &(dyn std::error::Error + 'static) = inner;
                if contains_serde_json_error(inner) {
                    return true;
                }
            }
        }

        current = err.source();
    }
    false
}

fn sanitize_json_error_message(message: &str) -> String {
    nova_core::sanitize_json_error_message(message)
}

#[cfg(test)]
mod json_error_sanitization_tests {
    use super::*;

    #[test]
    fn sanitize_anyhow_error_message_does_not_echo_string_values() {
        use anyhow::Context as _;

        let secret_suffix = "nova-dap-super-secret";
        let secret = format!("prefix\"{secret_suffix}");
        let serde_err = serde_json::from_value::<bool>(serde_json::json!(secret))
            .expect_err("expected type error");

        let err = Err::<(), _>(serde_err)
            .context("failed to parse JSON")
            .expect_err("expected anyhow error");

        let message = sanitize_anyhow_error_message(&err);
        assert!(
            !message.contains(secret_suffix),
            "expected sanitized anyhow error message to omit string values: {message}"
        );
        assert!(
            message.contains("<redacted>"),
            "expected sanitized anyhow error message to include redaction marker: {message}"
        );
    }

    #[test]
    fn sanitize_anyhow_error_message_does_not_echo_backticked_values() {
        use anyhow::Context as _;

        #[derive(Debug, serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct OnlyFoo {
            #[allow(dead_code)]
            foo: u32,
        }

        let secret_suffix = "nova-dap-anyhow-backticked-secret";
        let secret = format!("prefix`, expected {secret_suffix}");
        let json = format!(r#"{{"{secret}": 1}}"#);
        let serde_err =
            serde_json::from_str::<OnlyFoo>(&json).expect_err("expected unknown field error");
        let raw_message = serde_err.to_string();
        assert!(
            raw_message.contains(secret_suffix),
            "expected raw serde_json error string to include the backticked value so this test catches leaks: {raw_message}"
        );

        let err = Err::<(), _>(serde_err)
            .context("failed to parse JSON")
            .expect_err("expected anyhow error");

        let message = sanitize_anyhow_error_message(&err);
        assert!(
            !message.contains(secret_suffix),
            "expected sanitized anyhow error message to omit backticked values: {message}"
        );
        assert!(
            message.contains("<redacted>"),
            "expected sanitized anyhow error message to include redaction marker: {message}"
        );
    }

    #[test]
    fn sanitize_anyhow_error_message_does_not_echo_string_values_when_wrapped_in_io_error() {
        use anyhow::Context as _;

        let secret_suffix = "nova-dap-io-serde-secret";
        let secret = format!("prefix\"{secret_suffix}");
        let serde_err = serde_json::from_value::<bool>(serde_json::json!(secret))
            .expect_err("expected type error");
        let io_err = std::io::Error::new(std::io::ErrorKind::Other, serde_err);

        let err = Err::<(), _>(io_err)
            .context("failed to parse JSON")
            .expect_err("expected anyhow error");

        let message = sanitize_anyhow_error_message(&err);
        assert!(
            !message.contains(secret_suffix),
            "expected sanitized anyhow error message to omit string values: {message}"
        );
        assert!(
            message.contains("<redacted>"),
            "expected sanitized anyhow error message to include redaction marker: {message}"
        );
    }

    #[test]
    fn sanitize_anyhow_error_message_does_not_echo_backticked_values_when_wrapped_in_io_error() {
        use anyhow::Context as _;

        #[derive(Debug, serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct OnlyFoo {
            #[allow(dead_code)]
            foo: u32,
        }

        let secret_suffix = "nova-dap-anyhow-io-backticked-secret";
        let secret = format!("prefix`, expected {secret_suffix}");
        let json = format!(r#"{{"{secret}": 1}}"#);
        let serde_err =
            serde_json::from_str::<OnlyFoo>(&json).expect_err("expected unknown field error");
        let io_err = std::io::Error::new(std::io::ErrorKind::Other, serde_err);

        let err = Err::<(), _>(io_err)
            .context("failed to parse JSON")
            .expect_err("expected anyhow error");

        let message = sanitize_anyhow_error_message(&err);
        assert!(
            !message.contains(secret_suffix),
            "expected sanitized anyhow error message to omit backticked values: {message}"
        );
        assert!(
            message.contains("<redacted>"),
            "expected sanitized anyhow error message to include redaction marker: {message}"
        );
    }

    #[test]
    fn sanitize_anyhow_error_message_does_not_echo_string_values_when_wrapped_in_build_error() {
        use anyhow::Context as _;

        let secret_suffix = "nova-dap-build-error-secret";
        let secret = format!("prefix\"{secret_suffix}");
        let serde_err = serde_json::from_value::<bool>(serde_json::json!(secret))
            .expect_err("expected type error");
        let io_err = std::io::Error::new(std::io::ErrorKind::Other, serde_err);
        let build_err: nova_build::BuildError = io_err.into();

        let err = Err::<(), _>(build_err)
            .context("failed to parse JSON")
            .expect_err("expected anyhow error");

        let message = sanitize_anyhow_error_message(&err);
        assert!(
            !message.contains(secret_suffix),
            "expected sanitized anyhow error message to omit string values: {message}"
        );
        assert!(
            message.contains("<redacted>"),
            "expected sanitized anyhow error message to include redaction marker: {message}"
        );
    }

    #[test]
    fn sanitize_anyhow_error_message_does_not_echo_backticked_values_when_wrapped_in_build_error() {
        use anyhow::Context as _;

        #[derive(Debug, serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct OnlyFoo {
            #[allow(dead_code)]
            foo: u32,
        }

        let secret_suffix = "nova-dap-anyhow-build-backticked-secret";
        let secret = format!("prefix`, expected {secret_suffix}");
        let json = format!(r#"{{"{secret}": 1}}"#);
        let serde_err =
            serde_json::from_str::<OnlyFoo>(&json).expect_err("expected unknown field error");
        let io_err = std::io::Error::new(std::io::ErrorKind::Other, serde_err);
        let build_err: nova_build::BuildError = io_err.into();

        let err = Err::<(), _>(build_err)
            .context("failed to parse JSON")
            .expect_err("expected anyhow error");

        let message = sanitize_anyhow_error_message(&err);
        assert!(
            !message.contains(secret_suffix),
            "expected sanitized anyhow error message to omit backticked values: {message}"
        );
        assert!(
            message.contains("<redacted>"),
            "expected sanitized anyhow error message to include redaction marker: {message}"
        );
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
