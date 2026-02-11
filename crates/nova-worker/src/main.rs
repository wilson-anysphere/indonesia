use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use nova_bugreport::{install_panic_hook, PanicHookConfig};
use nova_config::{init_tracing_with_config, NovaConfig};
use nova_db::salsa::Database as SalsaDatabase;
use nova_db::{FileId, NovaHir, NovaInputs, NovaSyntax, SourceRootId};
use nova_remote_proto::v3::{
    CachedIndexInfo, Capabilities, CompressionAlgo, DiagnosticSeverity, Notification,
    ProtocolVersion, RemoteDiagnostic, Request, Response, RpcError as ProtoRpcError, RpcErrorCode,
    SupportedVersions,
};
use nova_remote_proto::{FileText, ShardId, ShardIndex, WorkerStats};
use nova_remote_rpc::{CancellationToken, RequestContext, RpcConnection, WorkerConfig};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::{watch, Mutex};
use tracing::{info, warn};

#[cfg(unix)]
use tokio::net::UnixStream;

#[cfg(windows)]
use tokio::net::windows::named_pipe::ClientOptions;

#[cfg(feature = "tls")]
mod tls;

const DEFAULT_MAX_RPC_BYTES: usize = nova_remote_proto::MAX_MESSAGE_BYTES;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{}", sanitize_anyhow_error_message(&err));
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let args = Args::parse()?;

    let config = NovaConfig::default();
    let _ = init_tracing_with_config(&config);
    install_panic_hook(
        PanicHookConfig {
            include_backtrace: config.logging.include_backtrace,
            ..Default::default()
        },
        Arc::new(|message| {
            let _ = writeln!(std::io::stderr(), "{message}");
        }),
    );

    let span = tracing::info_span!(
        "nova.worker",
        shard_id = args.shard_id,
        worker_id = tracing::field::Empty
    );
    let _guard = span.enter();

    info!(
        connect = ?args.connect,
        cache_dir = %args.cache_dir.display(),
        "starting worker"
    );

    match (&args.connect, args.auth_token.as_ref()) {
        (ConnectAddr::Tcp(addr), Some(_)) if !args.allow_insecure => {
            return Err(anyhow!(
                "refusing to connect to {addr} via plaintext TCP (`tcp:`) while an auth token is set. \
This would send the auth token and shard source code in cleartext. \
Use `tcp+tls:` or pass `--allow-insecure` for local testing."
            ));
        }
        (ConnectAddr::Tcp(addr), Some(_)) => {
            warn!(
                addr = %addr,
                "connecting via plaintext TCP with an auth token; this will send the token and shard source code in cleartext"
            );
        }
        (ConnectAddr::Tcp(addr), None) => {
            warn!(
                addr = %addr,
                "connecting via plaintext TCP (`tcp:`); traffic is unencrypted; prefer `tcp+tls:` for remote connections"
            );
        }
        _ => {}
    }

    let stream: BoxedStream = match args.connect {
        #[cfg(unix)]
        ConnectAddr::Unix(path) => Box::new(
            UnixStream::connect(path)
                .await
                .context("connect unix socket")?,
        ),
        #[cfg(windows)]
        ConnectAddr::NamedPipe(name) => {
            let name = normalize_pipe_name(&name);
            let mut attempts = 0u32;
            let client = loop {
                match ClientOptions::new().open(&name) {
                    Ok(client) => break client,
                    Err(err) if attempts < 50 => {
                        attempts += 1;
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                    Err(err) => {
                        return Err(err).with_context(|| format!("connect named pipe {name}"))
                    }
                }
            };
            Box::new(client)
        }
        ConnectAddr::Tcp(addr) => Box::new(TcpStream::connect(addr).await.context("connect tcp")?),
        #[cfg(feature = "tls")]
        ConnectAddr::TcpTls(addr) => {
            let tcp = TcpStream::connect(addr).await.context("connect tcp")?;
            Box::new(tls::connect(tcp, &args.tls).await?)
        }
    };

    let cached_index = match tokio::task::spawn_blocking({
        let cache_dir = args.cache_dir.clone();
        move || nova_cache::load_shard_index(&cache_dir, args.shard_id)
    })
    .await
    {
        Ok(Ok(index)) => index,
        Ok(Err(err)) => {
            warn!(error = ?err, "failed to load shard cache");
            None
        }
        Err(err) => {
            warn!(error = ?err, "failed to join shard cache task");
            None
        }
    };

    let max_rpc_bytes = clamp_max_rpc_bytes(args.max_rpc_bytes);
    let max_rpc_len: u32 = max_rpc_bytes
        .try_into()
        .unwrap_or_else(|_| u32::MAX.min(nova_remote_proto::MAX_MESSAGE_BYTES as u32));

    let hello = nova_remote_proto::v3::WorkerHello {
        shard_id: args.shard_id,
        auth_token: args.auth_token.clone(),
        supported_versions: SupportedVersions {
            min: ProtocolVersion::CURRENT,
            max: ProtocolVersion::CURRENT,
        },
        capabilities: Capabilities {
            supports_chunking: true,
            supports_cancel: true,
            supported_compression: vec![CompressionAlgo::None],
            max_frame_len: max_rpc_len,
            max_packet_len: max_rpc_len,
        },
        cached_index_info: cached_index.as_ref().map(CachedIndexInfo::from_index),
        worker_build: None,
    };

    let mut worker_cfg = WorkerConfig::new(hello);
    // If the user configured a smaller post-handshake frame cap, apply it pre-handshake as well.
    worker_cfg.pre_handshake_max_frame_len =
        worker_cfg.pre_handshake_max_frame_len.min(max_rpc_len);

    let (conn, welcome) = RpcConnection::handshake_as_worker_with_config(stream, worker_cfg)
        .await
        .map_err(|err| anyhow!("v3 handshake failed: {err}"))?;

    if welcome.shard_id != args.shard_id {
        return Err(anyhow!(
            "welcome shard mismatch: expected {}, got {}",
            args.shard_id,
            welcome.shard_id
        ));
    }

    span.record("worker_id", welcome.worker_id);
    info!(
        worker_id = welcome.worker_id,
        revision = welcome.revision,
        chosen_version = ?welcome.chosen_version,
        chosen_capabilities = ?welcome.chosen_capabilities,
        "connected to router"
    );

    let state = Arc::new(Mutex::new(WorkerState::new(
        args.shard_id,
        args.cache_dir.clone(),
        cached_index.as_ref(),
    )));
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

    conn.set_request_handler({
        let state = state.clone();
        let shutdown_tx = shutdown_tx.clone();
        move |ctx, request| {
            let state = state.clone();
            let shutdown_tx = shutdown_tx.clone();
            async move { handle_request(state, shutdown_tx, ctx, request).await }
        }
    });

    if let Some(index) = cached_index {
        if let Err(err) = conn.notify(Notification::CachedIndex(index)).await {
            warn!(error = ?err, "failed to send cached index notification");
        }
    }

    while shutdown_rx.changed().await.is_ok() {
        if *shutdown_rx.borrow() {
            break;
        }
    }

    // Best-effort: allow in-flight responses to flush before tearing down the runtime.
    tokio::time::sleep(Duration::from_millis(50)).await;
    if let Err(err) = conn.shutdown().await {
        warn!(error = ?err, "failed to close rpc connection");
    }

    info!("worker shutdown");
    Ok(())
}

fn sanitize_anyhow_error_message(err: &anyhow::Error) -> String {
    // `serde_json::Error` display strings can include user-provided scalar values (e.g.
    // `invalid type: string "..."`). Avoid echoing those values to stderr in the worker's
    // top-level error report.
    let message = format!("{err:#}");
    if err.chain().any(contains_serde_json_error) {
        sanitize_json_error_message(&message)
    } else {
        message
    }
}

fn contains_serde_json_error(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(err) = current {
        if err.is::<serde_json::Error>() {
            return true;
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

async fn handle_request(
    state: Arc<Mutex<WorkerState>>,
    shutdown_tx: watch::Sender<bool>,
    ctx: RequestContext,
    request: Request,
) -> std::result::Result<Response, ProtoRpcError> {
    if ctx.cancellation().is_cancelled() {
        return Err(cancelled_error());
    }

    match request {
        Request::LoadFiles { revision, files } => {
            let mut state = state.lock().await;
            state.revision = revision;
            state.apply_file_snapshot(files);
            Ok(Response::Ack)
        }
        Request::IndexShard { revision, files } => {
            let mut state = state.lock().await;
            state.revision = revision;
            state.apply_file_snapshot(files);

            if ctx.cancellation().is_cancelled() {
                return Err(cancelled_error());
            }
            let index = state.build_index(Some(ctx.cancellation())).await?;
            if ctx.cancellation().is_cancelled() {
                return Err(cancelled_error());
            }
            Ok(Response::ShardIndex(index))
        }
        Request::UpdateFile { revision, file } => {
            let mut state = state.lock().await;
            state.revision = revision;
            state.apply_file_update(file);

            if ctx.cancellation().is_cancelled() {
                return Err(cancelled_error());
            }
            let index = state.build_index(Some(ctx.cancellation())).await?;
            if ctx.cancellation().is_cancelled() {
                return Err(cancelled_error());
            }
            Ok(Response::ShardIndex(index))
        }
        Request::Diagnostics { path } => {
            // Best-effort: failures should not abort the caller. Return an empty diagnostics list
            // on any error/miss.
            let state = state.lock().await;
            let Some(&file_id) = state.path_to_file_id.get(&path) else {
                return Ok(Response::Diagnostics {
                    diagnostics: Vec::new(),
                });
            };

            let snap = state.db.snapshot();
            let text = snap.file_content(file_id);
            let parse = snap.parse_java(file_id);
            let mut diagnostics = Vec::new();
            for err in parse
                .errors
                .iter()
                .take(nova_remote_proto::MAX_DIAGNOSTICS_PER_MESSAGE)
            {
                let (line, column_utf16) = byte_offset_to_line_col(&text, err.range.start);
                diagnostics.push(RemoteDiagnostic {
                    severity: DiagnosticSeverity::Error,
                    line,
                    column: column_utf16,
                    message: err.message.clone(),
                });
            }
            Ok(Response::Diagnostics { diagnostics })
        }
        Request::GetWorkerStats => {
            let state = state.lock().await;
            Ok(Response::WorkerStats(state.worker_stats()))
        }
        Request::Shutdown => {
            let _ = shutdown_tx.send(true);
            Ok(Response::Shutdown)
        }
        Request::Unknown => Err(ProtoRpcError {
            code: RpcErrorCode::InvalidRequest,
            message: "unknown request".into(),
            retryable: false,
            details: None,
        }),
    }
}

/// Convert a byte offset within `text` into a 0-based (line, column) pair.
///
/// `line` is counted by the number of `\n` characters before `byte_offset`.
/// `column` is measured in UTF-16 code units since the last `\n`, matching the
/// LSP `Position.character` encoding.
fn byte_offset_to_line_col(text: &str, byte_offset: u32) -> (u32, u32) {
    let mut end = (byte_offset as usize).min(text.len());

    // `byte_offset` should usually already be a char boundary (it comes from our
    // parser's `TextRange`), but clamp defensively so we never slice into the
    // middle of a UTF-8 codepoint.
    while end > 0 && !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }

    let mut line: u32 = 0;
    let mut col_utf16: u32 = 0;

    for ch in text[..end].chars() {
        if ch == '\n' {
            line = line.saturating_add(1);
            col_utf16 = 0;
        } else {
            col_utf16 = col_utf16.saturating_add(ch.len_utf16() as u32);
        }
    }

    (line, col_utf16)
}

fn cancelled_error() -> ProtoRpcError {
    ProtoRpcError {
        code: RpcErrorCode::Cancelled,
        message: "request cancelled".into(),
        retryable: true,
        details: None,
    }
}

fn internal_error(err: anyhow::Error) -> ProtoRpcError {
    let contains_serde_json = err.chain().any(contains_serde_json_error);
    let message = err.to_string();
    let details = format!("{err:#}");
    ProtoRpcError {
        code: RpcErrorCode::Internal,
        message: if contains_serde_json {
            sanitize_json_error_message(&message)
        } else {
            message
        },
        retryable: false,
        details: Some(if contains_serde_json {
            sanitize_json_error_message(&details)
        } else {
            details
        }),
    }
}

fn clamp_max_rpc_bytes(max_rpc_bytes: usize) -> usize {
    max_rpc_bytes
        .min(u32::MAX as usize)
        .min(nova_remote_proto::MAX_MESSAGE_BYTES)
        .max(1)
}

#[derive(Clone)]
struct Args {
    connect: ConnectAddr,
    shard_id: ShardId,
    cache_dir: PathBuf,
    auth_token: Option<String>,
    allow_insecure: bool,
    max_rpc_bytes: usize,
    #[cfg(feature = "tls")]
    tls: Option<TlsArgs>,
}

impl fmt::Debug for Args {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = f.debug_struct("Args");
        s.field("connect", &self.connect)
            .field("shard_id", &self.shard_id)
            .field("cache_dir", &self.cache_dir)
            .field("auth_present", &self.auth_token.is_some())
            .field("allow_insecure", &self.allow_insecure)
            .field("max_rpc_bytes", &self.max_rpc_bytes);
        #[cfg(feature = "tls")]
        s.field("tls", &self.tls);
        s.finish()
    }
}

#[derive(Clone, Debug)]
enum ConnectAddr {
    #[cfg(unix)]
    Unix(PathBuf),
    #[cfg(windows)]
    NamedPipe(String),
    Tcp(SocketAddr),
    #[cfg(feature = "tls")]
    TcpTls(SocketAddr),
}

#[cfg(feature = "tls")]
#[derive(Clone, Debug)]
struct TlsArgs {
    ca_cert: PathBuf,
    domain: String,
    client_cert: Option<PathBuf>,
    client_key: Option<PathBuf>,
}

impl Args {
    fn parse() -> Result<Self> {
        let mut connect = None;
        let mut shard_id = None;
        let mut cache_dir = None;
        let mut auth_token = None;
        let mut auth_token_file: Option<PathBuf> = None;
        let mut auth_token_env: Option<String> = None;
        let mut allow_insecure = false;
        let mut max_rpc_bytes = DEFAULT_MAX_RPC_BYTES;
        let mut tls_ca_cert = None;
        let mut tls_domain = None;
        let mut tls_client_cert = None;
        let mut tls_client_key = None;

        let mut iter = std::env::args().skip(1);
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--allow-insecure" => allow_insecure = true,
                "--max-rpc-bytes" => {
                    max_rpc_bytes = iter
                        .next()
                        .ok_or_else(|| anyhow!("--max-rpc-bytes requires value"))?
                        .parse()
                        .context("parse --max-rpc-bytes")?;
                }
                "--connect" => {
                    connect = Some(
                        iter.next()
                            .ok_or_else(|| anyhow!("--connect requires value"))?,
                    )
                }
                "--shard-id" => {
                    shard_id = Some(
                        iter.next()
                            .ok_or_else(|| anyhow!("--shard-id requires value"))?
                            .parse()
                            .context("parse --shard-id")?,
                    )
                }
                "--cache-dir" => {
                    cache_dir = Some(PathBuf::from(
                        iter.next()
                            .ok_or_else(|| anyhow!("--cache-dir requires value"))?,
                    ))
                }
                "--auth-token" => {
                    auth_token = Some(
                        iter.next()
                            .ok_or_else(|| anyhow!("--auth-token requires value"))?,
                    )
                }
                "--auth-token-file" => {
                    auth_token_file =
                        Some(PathBuf::from(iter.next().ok_or_else(|| {
                            anyhow!("--auth-token-file requires value")
                        })?))
                }
                "--auth-token-env" => {
                    auth_token_env = Some(
                        iter.next()
                            .ok_or_else(|| anyhow!("--auth-token-env requires value"))?,
                    )
                }
                "--tls-ca-cert" => {
                    tls_ca_cert = Some(PathBuf::from(
                        iter.next()
                            .ok_or_else(|| anyhow!("--tls-ca-cert requires value"))?,
                    ))
                }
                "--tls-domain" => {
                    tls_domain = Some(
                        iter.next()
                            .ok_or_else(|| anyhow!("--tls-domain requires value"))?,
                    )
                }
                "--tls-client-cert" => {
                    tls_client_cert =
                        Some(PathBuf::from(iter.next().ok_or_else(|| {
                            anyhow!("--tls-client-cert requires value")
                        })?))
                }
                "--tls-client-key" => {
                    tls_client_key = Some(PathBuf::from(
                        iter.next()
                            .ok_or_else(|| anyhow!("--tls-client-key requires value"))?,
                    ))
                }
                _ => return Err(anyhow!("unknown argument: {arg}")),
            }
        }

        let connect = connect.ok_or_else(|| anyhow!("--connect is required"))?;
        let shard_id = shard_id.ok_or_else(|| anyhow!("--shard-id is required"))?;
        let cache_dir = cache_dir.ok_or_else(|| anyhow!("--cache-dir is required"))?;

        let auth_token = match (auth_token, auth_token_file, auth_token_env) {
            (None, None, None) => None,
            (Some(token), None, None) => Some(token),
            (None, Some(path), None) => {
                let token = std::fs::read_to_string(&path)
                    .with_context(|| format!("read --auth-token-file {}", path.display()))?;
                let token = token.trim().to_string();
                if token.is_empty() {
                    return Err(anyhow!("--auth-token-file {} was empty", path.display()));
                }
                Some(token)
            }
            (None, None, Some(var)) => {
                let token =
                    std::env::var(&var).with_context(|| format!("read --auth-token-env {var}"))?;
                let token = token.trim().to_string();
                if token.is_empty() {
                    return Err(anyhow!("--auth-token-env {var} was empty"));
                }
                Some(token)
            }
            _ => {
                return Err(anyhow!(
                    "--auth-token, --auth-token-file, and --auth-token-env are mutually exclusive"
                ))
            }
        };

        #[cfg(not(feature = "tls"))]
        if tls_ca_cert.is_some()
            || tls_domain.is_some()
            || tls_client_cert.is_some()
            || tls_client_key.is_some()
        {
            return Err(anyhow!(
                "TLS flags require building nova-worker with `--features tls`"
            ));
        }

        #[cfg(feature = "tls")]
        if tls_ca_cert.is_none() && (tls_client_cert.is_some() || tls_client_key.is_some()) {
            return Err(anyhow!(
                "--tls-client-cert/--tls-client-key cannot be used without --tls-ca-cert"
            ));
        }

        #[cfg(feature = "tls")]
        let tls = match (tls_ca_cert, tls_domain) {
            (Some(ca_cert), domain) => {
                let (client_cert, client_key) = match (tls_client_cert, tls_client_key) {
                    (None, None) => (None, None),
                    (Some(cert), Some(key)) => (Some(cert), Some(key)),
                    (Some(_), None) => {
                        return Err(anyhow!(
                            "--tls-client-key is required with --tls-client-cert"
                        ))
                    }
                    (None, Some(_)) => {
                        return Err(anyhow!(
                            "--tls-client-cert is required with --tls-client-key"
                        ))
                    }
                };
                Some(TlsArgs {
                    ca_cert,
                    domain: domain.unwrap_or_else(|| "localhost".into()),
                    client_cert,
                    client_key,
                })
            }
            (None, None) => None,
            _ => return Err(anyhow!("--tls-domain cannot be used without --tls-ca-cert")),
        };

        Ok(Self {
            connect: parse_connect_addr(&connect)?,
            shard_id,
            cache_dir,
            auth_token,
            allow_insecure,
            max_rpc_bytes,
            #[cfg(feature = "tls")]
            tls,
        })
    }
}

fn parse_connect_addr(raw: &str) -> Result<ConnectAddr> {
    let (scheme, rest) = raw
        .split_once(':')
        .ok_or_else(|| anyhow!("invalid connect addr {raw:?}"))?;
    match scheme {
        "unix" => {
            #[cfg(unix)]
            {
                Ok(ConnectAddr::Unix(PathBuf::from(rest)))
            }
            #[cfg(not(unix))]
            {
                Err(anyhow!("unix sockets are not supported on this platform"))
            }
        }
        "pipe" => {
            #[cfg(windows)]
            {
                Ok(ConnectAddr::NamedPipe(rest.to_string()))
            }
            #[cfg(not(windows))]
            {
                Err(anyhow!("named pipes are only supported on Windows"))
            }
        }
        "tcp" => Ok(ConnectAddr::Tcp(rest.parse().context("parse tcp addr")?)),
        "tcp+tls" => {
            #[cfg(feature = "tls")]
            {
                Ok(ConnectAddr::TcpTls(rest.parse().context("parse tcp addr")?))
            }
            #[cfg(not(feature = "tls"))]
            {
                Err(anyhow!(
                    "tcp+tls requires building nova-worker with `--features tls`"
                ))
            }
        }
        _ => Err(anyhow!("unsupported connect scheme {scheme:?}")),
    }
}

#[cfg(windows)]
fn normalize_pipe_name(name: &str) -> String {
    if name.starts_with(r"\\.\pipe\") || name.starts_with(r"\\?\pipe\") {
        name.to_string()
    } else {
        format!(r"\\.\pipe\{name}")
    }
}

struct WorkerState {
    shard_id: ShardId,
    cache_dir: PathBuf,
    revision: u64,
    index_generation: u64,
    db: SalsaDatabase,
    next_file_id: u32,
    path_to_file_id: HashMap<String, FileId>,
    files: BTreeMap<String, FileId>,
}

impl WorkerState {
    fn new(shard_id: ShardId, cache_dir: PathBuf, cached_index: Option<&ShardIndex>) -> Self {
        let index_generation = cached_index
            .map(|index| index.index_generation)
            .unwrap_or(0);
        Self {
            shard_id,
            cache_dir,
            revision: 0,
            index_generation,
            db: SalsaDatabase::new(),
            next_file_id: 0,
            path_to_file_id: HashMap::new(),
            files: BTreeMap::new(),
        }
    }

    fn worker_stats(&self) -> WorkerStats {
        WorkerStats {
            shard_id: self.shard_id,
            revision: self.revision,
            index_generation: self.index_generation,
            file_count: self.files.len().try_into().unwrap_or(u32::MAX),
        }
    }

    fn apply_file_snapshot(&mut self, files: Vec<FileText>) {
        let root = SourceRootId::from_raw(self.shard_id);
        let old_files: Vec<(String, FileId)> = self
            .files
            .iter()
            .map(|(path, file_id)| (path.clone(), *file_id))
            .collect();

        let mut new_files = BTreeMap::new();
        for file in files {
            let file_id = self.file_id_for_path(&file.path);
            self.db.set_file_exists(file_id, true);
            self.db.set_source_root(file_id, root);
            self.db.set_file_content(file_id, Arc::new(file.text));
            new_files.insert(file.path, file_id);
        }

        for (path, file_id) in old_files {
            if !new_files.contains_key(&path) {
                self.db.set_file_exists(file_id, false);
                self.db.set_file_content(file_id, Arc::new(String::new()));
            }
        }

        self.files = new_files;
    }

    fn apply_file_update(&mut self, file: FileText) {
        let root = SourceRootId::from_raw(self.shard_id);
        let file_id = self.file_id_for_path(&file.path);
        self.db.set_file_exists(file_id, true);
        self.db.set_source_root(file_id, root);
        self.db.set_file_content(file_id, Arc::new(file.text));
        self.files.insert(file.path, file_id);
    }

    fn file_id_for_path(&mut self, path: &str) -> FileId {
        if let Some(file_id) = self.path_to_file_id.get(path) {
            return *file_id;
        }

        let file_id = FileId::from_raw(self.next_file_id);
        self.next_file_id = self.next_file_id.saturating_add(1);
        self.path_to_file_id.insert(path.to_string(), file_id);
        file_id
    }

    async fn build_index(
        &mut self,
        cancel: Option<CancellationToken>,
    ) -> std::result::Result<ShardIndex, ProtoRpcError> {
        let next_index_generation = self.index_generation.saturating_add(1);
        let shard_id = self.shard_id;
        let revision = self.revision;
        let index_generation = next_index_generation;
        let cache_dir = self.cache_dir.clone();
        let db = self.db.clone();
        let files: Vec<(String, FileId)> = self
            .files
            .iter()
            .map(|(path, file_id)| (path.clone(), *file_id))
            .collect();

        let index = tokio::task::spawn_blocking(
            move || -> std::result::Result<ShardIndex, ProtoRpcError> {
                if cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
                    return Err(cancelled_error());
                }
                let symbols = build_symbols(&db, &files, cancel.as_ref())?;
                if cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
                    return Err(cancelled_error());
                }
                let index = ShardIndex {
                    shard_id,
                    revision,
                    index_generation,
                    symbols,
                };
                if cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
                    return Err(cancelled_error());
                }
                nova_cache::save_shard_index(&cache_dir, &index)
                    .map_err(|err| internal_error(anyhow!(err).context("write shard cache")))?;
                Ok(index)
            },
        )
        .await
        .map_err(|err| internal_error(anyhow!(err).context("join shard index task")))??;

        // Only advance the generation if we produced a complete index.
        self.index_generation = next_index_generation;

        Ok(index)
    }
}

fn build_symbols(
    db: &SalsaDatabase,
    files: &[(String, FileId)],
    cancel: Option<&CancellationToken>,
) -> std::result::Result<Vec<nova_remote_proto::Symbol>, ProtoRpcError> {
    let snap = db.snapshot();
    let mut symbols = Vec::new();
    for (path, file_id) in files {
        if cancel.is_some_and(|c| c.is_cancelled()) {
            return Err(cancelled_error());
        }

        let text = snap.file_content(*file_id);
        let line_index = nova_core::LineIndex::new(&text);
        let tree = snap.hir_item_tree(*file_id);

        fn push_symbol(
            out: &mut Vec<nova_remote_proto::Symbol>,
            line_index: &nova_core::LineIndex,
            text: &str,
            name: &str,
            path: &str,
            name_start: usize,
        ) {
            let offset_u32 = u32::try_from(name_start).unwrap_or(0);
            let pos = line_index.position(text, nova_core::TextSize::from(offset_u32));
            out.push(nova_remote_proto::Symbol {
                name: name.to_string(),
                path: path.to_string(),
                line: pos.line,
                column: pos.character,
            });
        }

        fn collect_member_symbols(
            tree: &nova_hir::item_tree::ItemTree,
            members: &[nova_hir::item_tree::Member],
            line_index: &nova_core::LineIndex,
            text: &str,
            path: &str,
            out: &mut Vec<nova_remote_proto::Symbol>,
        ) {
            for member in members {
                match member {
                    nova_hir::item_tree::Member::Field(id) => {
                        let data = tree.field(*id);
                        push_symbol(
                            out,
                            line_index,
                            text,
                            &data.name,
                            path,
                            data.name_range.start,
                        );
                    }
                    nova_hir::item_tree::Member::Method(id) => {
                        let data = tree.method(*id);
                        push_symbol(
                            out,
                            line_index,
                            text,
                            &data.name,
                            path,
                            data.name_range.start,
                        );
                    }
                    nova_hir::item_tree::Member::Constructor(id) => {
                        let data = tree.constructor(*id);
                        push_symbol(
                            out,
                            line_index,
                            text,
                            &data.name,
                            path,
                            data.name_range.start,
                        );
                    }
                    nova_hir::item_tree::Member::Initializer(_) => {}
                    nova_hir::item_tree::Member::Type(item) => collect_item_symbols(
                        tree, *item, line_index, text, path, out,
                    ),
                }
            }
        }

        fn collect_item_symbols(
            tree: &nova_hir::item_tree::ItemTree,
            item: nova_hir::item_tree::Item,
            line_index: &nova_core::LineIndex,
            text: &str,
            path: &str,
            out: &mut Vec<nova_remote_proto::Symbol>,
        ) {
            match item {
                nova_hir::item_tree::Item::Class(id) => {
                    let data = tree.class(id);
                    push_symbol(
                        out,
                        line_index,
                        text,
                        &data.name,
                        path,
                        data.name_range.start,
                    );
                    collect_member_symbols(
                        tree,
                        &data.members,
                        line_index,
                        text,
                        path,
                        out,
                    );
                }
                nova_hir::item_tree::Item::Interface(id) => {
                    let data = tree.interface(id);
                    push_symbol(
                        out,
                        line_index,
                        text,
                        &data.name,
                        path,
                        data.name_range.start,
                    );
                    collect_member_symbols(
                        tree,
                        &data.members,
                        line_index,
                        text,
                        path,
                        out,
                    );
                }
                nova_hir::item_tree::Item::Enum(id) => {
                    let data = tree.enum_(id);
                    push_symbol(
                        out,
                        line_index,
                        text,
                        &data.name,
                        path,
                        data.name_range.start,
                    );
                    collect_member_symbols(
                        tree,
                        &data.members,
                        line_index,
                        text,
                        path,
                        out,
                    );
                }
                nova_hir::item_tree::Item::Record(id) => {
                    let data = tree.record(id);
                    push_symbol(
                        out,
                        line_index,
                        text,
                        &data.name,
                        path,
                        data.name_range.start,
                    );
                    collect_member_symbols(
                        tree,
                        &data.members,
                        line_index,
                        text,
                        path,
                        out,
                    );
                }
                nova_hir::item_tree::Item::Annotation(id) => {
                    let data = tree.annotation(id);
                    push_symbol(
                        out,
                        line_index,
                        text,
                        &data.name,
                        path,
                        data.name_range.start,
                    );
                    collect_member_symbols(
                        tree,
                        &data.members,
                        line_index,
                        text,
                        path,
                        out,
                    );
                }
            }
        }

        for item in tree.items.iter() {
            collect_item_symbols(tree.as_ref(), *item, &line_index, &text, path, &mut symbols);
        }
    }

    symbols.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.line.cmp(&b.line))
            .then_with(|| a.column.cmp(&b.column))
    });
    symbols.dedup_by(|a, b| a.name == b.name && a.path == b.path);
    Ok(symbols)
}

type BoxedStream = Box<dyn AsyncReadWrite>;

trait AsyncReadWrite: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T> AsyncReadWrite for T where T: AsyncRead + AsyncWrite + Unpin + Send + 'static {}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn sanitize_anyhow_error_message_does_not_echo_string_values() {
        use anyhow::Context as _;

        let secret_suffix = "nova-worker-super-secret";
        let secret = format!("prefix\"{secret_suffix}");
        let serde_err = serde_json::from_value::<bool>(serde_json::json!(secret))
            .expect_err("expected type error");

        let err = Err::<(), _>(serde_err)
            .context("failed to parse JSON")
            .expect_err("expected anyhow error");

        let message = sanitize_anyhow_error_message(&err);
        assert!(
            !message.contains(secret_suffix),
            "expected sanitized error message to omit string values: {message}"
        );
        assert!(
            message.contains("<redacted>"),
            "expected sanitized error message to include redaction marker: {message}"
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

        let secret_suffix = "nova-worker-anyhow-backticked-secret";
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
            "expected sanitized error message to omit backticked values: {message}"
        );
        assert!(
            message.contains("<redacted>"),
            "expected sanitized error message to include redaction marker: {message}"
        );
    }

    #[test]
    fn sanitize_anyhow_error_message_does_not_echo_string_values_when_wrapped_in_io_error() {
        use anyhow::Context as _;

        let secret_suffix = "nova-worker-io-serde-secret";
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
            "expected sanitized error message to omit string values: {message}"
        );
        assert!(
            message.contains("<redacted>"),
            "expected sanitized error message to include redaction marker: {message}"
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

        let secret_suffix = "nova-worker-anyhow-io-backticked-secret";
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
            "expected sanitized error message to omit backticked values: {message}"
        );
        assert!(
            message.contains("<redacted>"),
            "expected sanitized error message to include redaction marker: {message}"
        );
    }

    #[test]
    fn internal_error_does_not_echo_serde_json_string_values() {
        use anyhow::Context as _;

        let secret_suffix = "nova-worker-internal-error-secret";
        let secret = format!("prefix\"{secret_suffix}");
        let serde_err = serde_json::from_value::<bool>(serde_json::json!(secret))
            .expect_err("expected type error");

        let err = Err::<(), _>(serde_err)
            .context("failed to parse JSON")
            .expect_err("expected anyhow error");

        let rpc_err = internal_error(err);
        let message = format!(
            "{} {}",
            rpc_err.message,
            rpc_err.details.unwrap_or_default()
        );
        assert!(
            !message.contains(secret_suffix),
            "expected internal RPC error to omit serde_json string values: {message}"
        );
        assert!(
            message.contains("<redacted>"),
            "expected internal RPC error to include redaction marker: {message}"
        );
    }

    #[test]
    fn internal_error_does_not_echo_serde_json_backticked_values() {
        use anyhow::Context as _;

        #[derive(Debug, serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct OnlyFoo {
            #[allow(dead_code)]
            foo: u32,
        }

        let secret_suffix = "nova-worker-internal-error-backticked-secret";
        let secret = format!("prefix`, expected {secret_suffix}");
        let json = format!(r#"{{"{secret}": 1}}"#);
        let serde_err =
            serde_json::from_str::<OnlyFoo>(&json).expect_err("expected unknown field error");

        let err = Err::<(), _>(serde_err)
            .context("failed to parse JSON")
            .expect_err("expected anyhow error");

        let rpc_err = internal_error(err);
        let message = format!(
            "{} {}",
            rpc_err.message,
            rpc_err.details.unwrap_or_default()
        );
        assert!(
            !message.contains(secret_suffix),
            "expected internal RPC error to omit serde_json backticked values: {message}"
        );
        assert!(
            message.contains("<redacted>"),
            "expected internal RPC error to include redaction marker: {message}"
        );
    }

    #[test]
    fn internal_error_does_not_echo_serde_json_string_values_when_wrapped_in_io_error() {
        use anyhow::Context as _;

        let secret_suffix = "nova-worker-internal-io-secret";
        let secret = format!("prefix\"{secret_suffix}");
        let serde_err = serde_json::from_value::<bool>(serde_json::json!(secret))
            .expect_err("expected type error");
        let io_err = std::io::Error::new(std::io::ErrorKind::Other, serde_err);

        let err = Err::<(), _>(io_err)
            .context("failed to parse JSON")
            .expect_err("expected anyhow error");

        let rpc_err = internal_error(err);
        let message = format!(
            "{} {}",
            rpc_err.message,
            rpc_err.details.unwrap_or_default()
        );
        assert!(
            !message.contains(secret_suffix),
            "expected internal RPC error to omit serde_json string values: {message}"
        );
        assert!(
            message.contains("<redacted>"),
            "expected internal RPC error to include redaction marker: {message}"
        );
    }

    #[test]
    fn internal_error_does_not_echo_serde_json_backticked_values_when_wrapped_in_io_error() {
        use anyhow::Context as _;

        #[derive(Debug, serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct OnlyFoo {
            #[allow(dead_code)]
            foo: u32,
        }

        let secret_suffix = "nova-worker-internal-io-backticked-secret";
        let secret = format!("prefix`, expected {secret_suffix}");
        let json = format!(r#"{{"{secret}": 1}}"#);
        let serde_err =
            serde_json::from_str::<OnlyFoo>(&json).expect_err("expected unknown field error");
        let io_err = std::io::Error::new(std::io::ErrorKind::Other, serde_err);

        let err = Err::<(), _>(io_err)
            .context("failed to parse JSON")
            .expect_err("expected anyhow error");

        let rpc_err = internal_error(err);
        let message = format!(
            "{} {}",
            rpc_err.message,
            rpc_err.details.unwrap_or_default()
        );
        assert!(
            !message.contains(secret_suffix),
            "expected internal RPC error to omit serde_json backticked values: {message}"
        );
        assert!(
            message.contains("<redacted>"),
            "expected internal RPC error to include redaction marker: {message}"
        );
    }

    #[test]
    fn args_debug_does_not_expose_auth_token() {
        let token = "super-secret-token";
        let tmp = TempDir::new().expect("create temp dir");
        let args = Args {
            connect: ConnectAddr::Tcp("127.0.0.1:0".parse().unwrap()),
            shard_id: 1,
            cache_dir: tmp.path().to_path_buf(),
            auth_token: Some(token.to_string()),
            allow_insecure: false,
            max_rpc_bytes: DEFAULT_MAX_RPC_BYTES,
            #[cfg(feature = "tls")]
            tls: None,
        };

        let output = format!("{args:?}");
        assert!(
            !output.contains(token),
            "nova-worker Args debug output leaked auth token: {output}"
        );
        assert!(
            output.contains("auth_present"),
            "nova-worker Args debug output should include auth presence indicator: {output}"
        );
    }

    #[test]
    fn byte_offset_to_line_col_uses_utf16_columns() {
        // `é` is 2 bytes in UTF-8 but 1 code unit in UTF-16.
        let text = "aéx";
        let byte_offset = text.find('x').expect("find x") as u32;
        assert_eq!(byte_offset, 3);

        let (line, col) = byte_offset_to_line_col(text, byte_offset);
        assert_eq!(line, 0);
        assert_eq!(col, 2);

        // 😀 is 4 bytes in UTF-8 but 2 code units in UTF-16.
        let text = "a😀b";
        let byte_offset = text.find('b').expect("find b") as u32;
        assert_eq!(byte_offset, 5);

        let (line, col) = byte_offset_to_line_col(text, byte_offset);
        assert_eq!(line, 0);
        assert_eq!(col, 3);
    }

    fn parse_executions(db: &SalsaDatabase) -> u64 {
        db.query_stats()
            .by_query
            .get("parse_java")
            .map(|stat| stat.executions)
            .unwrap_or(0)
    }

    #[tokio::test]
    async fn indexing_reuses_salsa_results_across_file_updates() -> Result<()> {
        let tmp = TempDir::new().context("create temp dir")?;
        let mut state = WorkerState::new(0, tmp.path().to_path_buf(), None);

        state.revision = 1;
        state.apply_file_snapshot(vec![
            FileText {
                path: "A.java".into(),
                text: "class Alpha {}".into(),
            },
            FileText {
                path: "B.java".into(),
                text: "class Beta {}".into(),
            },
        ]);

        let _ = state
            .build_index(None)
            .await
            .map_err(|err| anyhow!(err.message))?;
        let first_parse = parse_executions(&state.db);
        assert_eq!(
            first_parse, 2,
            "expected initial index to parse_java both files"
        );

        state.revision = 2;
        state.apply_file_update(FileText {
            path: "A.java".into(),
            text: "class Alpha { int x; }".into(),
        });
        let _ = state
            .build_index(None)
            .await
            .map_err(|err| anyhow!(err.message))?;

        let second_parse = parse_executions(&state.db);
        assert_eq!(
            second_parse,
            first_parse + 1,
            "expected only the updated file to reparse"
        );

        Ok(())
    }
}
