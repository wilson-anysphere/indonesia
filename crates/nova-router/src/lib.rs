use std::cell::RefCell;
use std::collections::{BinaryHeap, HashMap};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context};
use nova_bugreport::{install_panic_hook, PanicHookConfig};
use nova_config::{init_tracing_with_config, NovaConfig};
use nova_fuzzy::{
    FuzzyMatcher, MatchScore, TrigramCandidateScratch, TrigramIndex, TrigramIndexBuilder,
};
use nova_remote_proto::v3::{
    HandshakeReject, Notification, RejectCode, RemoteDiagnostic, Request, Response,
};
use nova_remote_proto::{FileText, ShardId, ShardIndex, Symbol, WorkerId, WorkerStats};
use nova_remote_rpc::{
    PendingCall, RouterAdmission, RouterConfig as RpcRouterConfig, RpcConnection,
};
use nova_scheduler::{CancellationToken, Cancelled, Scheduler, SchedulerConfig, TaskError};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader};
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::sync::{watch, Mutex, Notify, OwnedSemaphorePermit, RwLock, Semaphore};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{timeout, Duration, Instant};
use tracing::{error, info, warn};

#[cfg(unix)]
use tokio::net::UnixListener;

mod ipc_security;

mod supervisor;
#[cfg(feature = "tls")]
pub mod tls;

use supervisor::RestartBackoff;

pub type Result<T> = anyhow::Result<T>;

fn rpc_cancelled_error() -> anyhow::Error {
    anyhow!(nova_remote_rpc::RpcError::Canceled)
}

/// Initialize structured logging and install the global panic hook used by Nova.
///
/// `nova-router` is typically embedded within `nova-lsp`, which is responsible
/// for calling this early during startup. Standalone router binaries can use
/// this helper directly.
pub fn init_observability(
    config: &NovaConfig,
    notifier: Arc<dyn Fn(&str) + Send + Sync + 'static>,
) {
    let _ = init_tracing_with_config(config);
    install_panic_hook(
        PanicHookConfig {
            include_backtrace: config.logging.include_backtrace,
            ..Default::default()
        },
        notifier,
    );
}

const WORKSPACE_SYMBOL_LIMIT: usize = 200;
const FALLBACK_SCAN_LIMIT: usize = 50_000;
const MAX_CONCURRENT_HANDSHAKES: usize = 128;
const WORKER_RPC_WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const WORKER_RPC_READ_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const WORKER_RPC_CANCEL_TIMEOUT: Duration = Duration::from_millis(200);
const WORKER_SHUTDOWN_RPC_TIMEOUT: Duration = Duration::from_secs(2);

// Collecting a full file snapshot (`Vec<FileText>`) can be very memory intensive for large shards.
// Limit how many shards can be in the "collect + send snapshot" phase concurrently to keep peak
// memory bounded while still allowing multiple workers to index in parallel once they have their
// snapshot.
const MAX_CONCURRENT_SHARD_FILE_SNAPSHOTS: usize = 2;

const WORKER_RESTART_BACKOFF_INITIAL: Duration = Duration::from_millis(50);
const WORKER_RESTART_BACKOFF_MAX: Duration = Duration::from_secs(5);
const WORKER_SESSION_RESET_BACKOFF_AFTER: Duration = Duration::from_secs(10);
const WORKER_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const WORKER_WAIT_TIMEOUT: Duration = Duration::from_secs(20);
const WORKER_KILL_TIMEOUT: Duration = Duration::from_secs(2);
const WORKER_RESTART_JITTER_DIVISOR: u32 = 4;

/// Maximum number of bytes allowed for the first message on a new connection (`WorkerHello`).
///
/// Unauthenticated clients should never be able to force the router to allocate large buffers.
pub const MAX_HELLO_BYTES: usize = 1024 * 1024; // 1 MiB

/// Default maximum number of bytes accepted for framed RPC messages after authentication.
pub const DEFAULT_MAX_RPC_BYTES: usize = nova_remote_proto::MAX_MESSAGE_BYTES;

/// Default maximum number of concurrent in-flight connection handshakes.
pub const DEFAULT_MAX_INFLIGHT_HANDSHAKES: usize = MAX_CONCURRENT_HANDSHAKES;

/// Default maximum number of active worker connections.
pub const DEFAULT_MAX_WORKER_CONNECTIONS: usize = 1024;

#[derive(Clone, Debug)]
pub struct SourceRoot {
    pub path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct WorkspaceLayout {
    pub source_roots: Vec<SourceRoot>,
}

#[derive(Clone, Debug)]
pub enum ListenAddr {
    #[cfg(unix)]
    Unix(PathBuf),
    #[cfg(windows)]
    NamedPipe(String),
    Tcp(TcpListenAddr),
}

impl ListenAddr {
    pub fn as_worker_connect_arg(&self) -> String {
        match self {
            #[cfg(unix)]
            ListenAddr::Unix(path) => format!("unix:{}", path.display()),
            #[cfg(windows)]
            ListenAddr::NamedPipe(name) => format!("pipe:{name}"),
            ListenAddr::Tcp(cfg) => match cfg {
                TcpListenAddr::Plain(addr) => format!("tcp:{addr}"),
                #[cfg(feature = "tls")]
                TcpListenAddr::Tls { addr, .. } => format!("tcp+tls:{addr}"),
            },
        }
    }
}

#[derive(Clone, Debug)]
pub enum TcpListenAddr {
    Plain(SocketAddr),
    #[cfg(feature = "tls")]
    Tls {
        addr: SocketAddr,
        config: tls::TlsServerConfig,
    },
}

#[derive(Clone)]
pub struct DistributedRouterConfig {
    pub listen_addr: ListenAddr,
    pub worker_command: PathBuf,
    pub cache_dir: PathBuf,
    pub auth_token: Option<String>,
    /// Allow binding plaintext TCP sockets / connecting over plaintext TCP.
    ///
    /// Plaintext TCP is insecure because it exposes source code and (when enabled) auth tokens to
    /// the network. Nova defaults to requiring TLS for remote TCP connections.
    ///
    /// This flag exists as an explicit escape hatch for local development and tests.
    pub allow_insecure_tcp: bool,
    /// Maximum message size accepted for authenticated RPC traffic.
    pub max_rpc_bytes: usize,
    /// Maximum number of concurrent in-flight connection handshakes.
    pub max_inflight_handshakes: usize,
    /// Maximum number of active worker connections.
    pub max_worker_connections: usize,
    #[cfg(feature = "tls")]
    pub tls_client_cert_fingerprint_allowlist: TlsClientCertFingerprintAllowlist,
    /// If true, the router spawns `nova-worker` processes locally (multi-process mode).
    ///
    /// If false, workers are expected to be started externally (e.g. on remote machines)
    /// and connect to `listen_addr` via RPC.
    pub spawn_workers: bool,
}

impl std::fmt::Debug for DistributedRouterConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = f.debug_struct("DistributedRouterConfig");
        s.field("listen_addr", &self.listen_addr)
            .field("worker_command", &self.worker_command)
            .field("cache_dir", &self.cache_dir)
            .field("auth_present", &self.auth_token.is_some())
            .field("allow_insecure_tcp", &self.allow_insecure_tcp)
            .field("max_rpc_bytes", &self.max_rpc_bytes)
            .field("max_inflight_handshakes", &self.max_inflight_handshakes)
            .field("max_worker_connections", &self.max_worker_connections)
            .field("spawn_workers", &self.spawn_workers);
        #[cfg(feature = "tls")]
        s.field(
            "tls_client_cert_fingerprint_allowlist",
            &self.tls_client_cert_fingerprint_allowlist,
        );
        s.finish()
    }
}

#[cfg(feature = "tls")]
#[derive(Clone, Debug, Default)]
pub struct TlsClientCertFingerprintAllowlist {
    /// Fingerprints allowed to connect to any shard.
    ///
    /// If non-empty, the allowlist is enforced for all shards (connections are rejected unless the
    /// presented client certificate fingerprint appears in this list or the shard-specific list).
    pub global: Vec<String>,
    /// Per-shard allowlists. If a shard is present in this map, connections for that shard are
    /// rejected unless the client's certificate fingerprint is listed (or present in `global`).
    pub shards: HashMap<ShardId, Vec<String>>,
}

#[cfg(feature = "tls")]
fn normalize_tls_client_cert_fingerprint(value: &str) -> Result<String> {
    const OPENSSL_PREFIX: &str = "SHA256 Fingerprint=";

    let mut s = value.trim();
    if let Some(prefix) = s.get(..OPENSSL_PREFIX.len()) {
        if prefix.eq_ignore_ascii_case(OPENSSL_PREFIX) {
            s = &s[OPENSSL_PREFIX.len()..];
        }
    }

    let mut out = String::with_capacity(64);
    for ch in s.chars() {
        if ch == ':' || ch.is_ascii_whitespace() {
            continue;
        }
        if !ch.is_ascii_hexdigit() {
            return Err(anyhow!(
                "invalid TLS client certificate fingerprint {value:?}: expected 64 hex characters"
            ));
        }
        out.push(ch.to_ascii_lowercase());
    }

    if out.len() != 64 {
        return Err(anyhow!(
            "invalid TLS client certificate fingerprint {value:?}: expected 64 hex characters, got {}",
            out.len()
        ));
    }

    Ok(out)
}

#[cfg(feature = "tls")]
impl TlsClientCertFingerprintAllowlist {
    fn normalize_in_place(&mut self) -> Result<()> {
        for fp in &mut self.global {
            let original = fp.clone();
            *fp = normalize_tls_client_cert_fingerprint(&original).with_context(|| {
                format!("normalize global TLS client certificate fingerprint {original:?}")
            })?;
        }

        for (shard_id, fingerprints) in &mut self.shards {
            for fp in fingerprints {
                let original = fp.clone();
                *fp = normalize_tls_client_cert_fingerprint(&original).with_context(|| {
                    format!("normalize TLS client certificate fingerprint for shard {shard_id}: {original:?}")
                })?;
            }
        }

        Ok(())
    }
}

#[cfg(all(test, feature = "tls"))]
mod tls_allowlist_tests {
    use super::*;

    #[test]
    fn tls_client_cert_fingerprint_normalization_accepts_openssl_format() {
        let expected = "ab".repeat(32);
        let mut openssl = String::from("SHA256 Fingerprint=");
        for i in 0..32 {
            if i > 0 {
                openssl.push(':');
            }
            // Uppercase to ensure case-insensitive parsing/canonicalization.
            openssl.push_str("AB");
        }

        let mut allowlist = TlsClientCertFingerprintAllowlist {
            global: vec![openssl],
            shards: HashMap::new(),
        };

        allowlist.normalize_in_place().unwrap();
        assert_eq!(allowlist.global, vec![expected]);
    }

    #[test]
    fn tls_client_cert_fingerprint_normalization_rejects_invalid_values() {
        let mut allowlist = TlsClientCertFingerprintAllowlist {
            global: vec!["not-a-fingerprint".to_string()],
            shards: HashMap::new(),
        };

        assert!(allowlist.normalize_in_place().is_err());
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum WorkerIdentity {
    /// No authenticated identity is available (Unix socket, plain TCP, or TLS without client auth).
    Unauthenticated,
    #[cfg(feature = "tls")]
    TlsClientCertFingerprint(String),
}

#[cfg(feature = "tls")]
impl WorkerIdentity {
    fn tls_client_cert_fingerprint(&self) -> Option<&str> {
        match self {
            WorkerIdentity::TlsClientCertFingerprint(fp) => Some(fp.as_str()),
            _ => None,
        }
    }
}

impl DistributedRouterConfig {
    /// Build a `DistributedRouterConfig` suitable for local IPC with locally spawned workers.
    ///
    /// This helper keeps `nova-lsp` and other frontends from needing to know about optional TLS
    /// fields on the config struct (which vary behind `nova-router`'s `tls` feature). It also
    /// centralizes the conservative defaults used by Nova:
    ///
    /// - `spawn_workers = true`
    /// - `auth_token = None`
    /// - `allow_insecure_tcp = false`
    /// - `max_rpc_bytes = DEFAULT_MAX_RPC_BYTES`
    /// - `max_inflight_handshakes = DEFAULT_MAX_INFLIGHT_HANDSHAKES`
    /// - `max_worker_connections = DEFAULT_MAX_WORKER_CONNECTIONS`
    pub fn local_ipc(listen_addr: ListenAddr, worker_command: PathBuf, cache_dir: PathBuf) -> Self {
        Self {
            listen_addr,
            worker_command,
            cache_dir,
            auth_token: None,
            allow_insecure_tcp: false,
            max_rpc_bytes: DEFAULT_MAX_RPC_BYTES,
            max_inflight_handshakes: DEFAULT_MAX_INFLIGHT_HANDSHAKES,
            max_worker_connections: DEFAULT_MAX_WORKER_CONNECTIONS,
            #[cfg(feature = "tls")]
            tls_client_cert_fingerprint_allowlist: Default::default(),
            spawn_workers: true,
        }
    }

    fn validate(&self) -> Result<()> {
        #[cfg(feature = "tls")]
        if self.spawn_workers
            && matches!(self.listen_addr, ListenAddr::Tcp(TcpListenAddr::Tls { .. }))
        {
            return Err(anyhow!(
                "`spawn_workers = true` is not supported with a `tcp+tls:` listen address. \
The router does not yet have a way to pass TLS client configuration (CA cert, SNI domain, optional client cert/key) to locally spawned workers. \
Use a local IPC transport (Unix socket / named pipe) or set `spawn_workers = false` and start remote workers with the appropriate TLS flags."
            ));
        }

        #[cfg(feature = "tls")]
        {
            let allowlist = &self.tls_client_cert_fingerprint_allowlist;
            let allowlist_configured = !allowlist.global.is_empty() || !allowlist.shards.is_empty();

            if allowlist_configured {
                match &self.listen_addr {
                    ListenAddr::Tcp(TcpListenAddr::Tls { config, .. }) => {
                        if config.client_ca_path.is_none() || !config.require_client_auth {
                            return Err(anyhow!(
                                "TLS client certificate fingerprint allowlist requires mTLS client verification. \
Configure the router TLS server with a client CA certificate (TlsServerConfig::with_client_ca_cert)."
                            ));
                        }
                    }
                    ListenAddr::Tcp(TcpListenAddr::Plain(addr)) => {
                        return Err(anyhow!(
                            "TLS client certificate fingerprint allowlist requires TLS (`tcp+tls:`); got plaintext TCP listen addr {addr}"
                        ));
                    }
                    #[cfg(unix)]
                    ListenAddr::Unix(_) => {
                        return Err(anyhow!(
                            "TLS client certificate fingerprint allowlist requires TCP+TLS (`tcp+tls:`); local IPC transports do not provide TLS identities"
                        ));
                    }
                    #[cfg(windows)]
                    ListenAddr::NamedPipe(_) => {
                        return Err(anyhow!(
                            "TLS client certificate fingerprint allowlist requires TCP+TLS (`tcp+tls:`); local IPC transports do not provide TLS identities"
                        ));
                    }
                }
            }
        }

        let ListenAddr::Tcp(tcp) = &self.listen_addr else {
            return Ok(());
        };

        let addr = match tcp {
            TcpListenAddr::Plain(addr) => *addr,
            #[cfg(feature = "tls")]
            TcpListenAddr::Tls { addr, .. } => *addr,
        };

        // If TLS is configured, remote safety is handled by the transport.
        if !matches!(tcp, TcpListenAddr::Plain(_)) {
            return Ok(());
        }

        if self.allow_insecure_tcp {
            return Ok(());
        }

        let non_loopback = !addr.ip().is_loopback();

        if self.auth_token.is_some() {
            return Err(anyhow!(
                "refusing to start distributed router with plaintext TCP (`tcp:`) while an auth token is configured. \
Plaintext TCP would expose the auth token and shard source code in cleartext. \
Use TLS (`tcp+tls:`; build with `--features tls`) or explicitly opt in with `allow_insecure_tcp: true` for local testing."
            ));
        }

        if non_loopback {
            return Err(anyhow!(
                "refusing to listen on insecure plaintext TCP address {addr}. \
This address is not loopback, so workers may connect over the network and all RPC traffic (including source code) would be unencrypted. \
Use TLS (`tcp+tls:`; build with `--features tls`) or explicitly opt in with `allow_insecure_tcp: true` for development/testing."
            ));
        }

        Ok(())
    }
}

/// QueryRouter is the coordination point described in `docs/04-incremental-computation.md`.
///
/// In this MVP it is responsible for:
/// - partitioning work by source root (shard)
/// - delegating indexing to worker processes over a simple RPC transport
/// - maintaining a router-local global symbol index (built from per-shard shard indexes) and
///   answering workspace symbol queries locally (no per-query RPC fanout)
pub struct QueryRouter {
    inner: RouterMode,
}

impl std::fmt::Debug for QueryRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueryRouter").finish_non_exhaustive()
    }
}

enum RouterMode {
    InProcess(InProcessRouter),
    Distributed(DistributedRouter),
}

impl QueryRouter {
    pub fn new_in_process(layout: WorkspaceLayout) -> Self {
        Self {
            inner: RouterMode::InProcess(InProcessRouter::new(layout)),
        }
    }

    pub async fn new_distributed(
        config: DistributedRouterConfig,
        layout: WorkspaceLayout,
    ) -> Result<Self> {
        Ok(Self {
            inner: RouterMode::Distributed(DistributedRouter::new(config, layout).await?),
        })
    }

    pub async fn bound_listen_addr(&self) -> Option<ListenAddr> {
        match &self.inner {
            RouterMode::InProcess(_) => None,
            RouterMode::Distributed(router) => router.bound_listen_addr().await,
        }
    }

    pub async fn index_workspace_cancelable(&self, cancel: CancellationToken) -> Result<()> {
        match &self.inner {
            RouterMode::InProcess(router) => router.index_workspace_cancelable(cancel).await,
            RouterMode::Distributed(router) => router.index_workspace_cancelable(cancel).await,
        }
    }

    pub async fn index_workspace(&self) -> Result<()> {
        self.index_workspace_cancelable(CancellationToken::new())
            .await
    }

    pub async fn update_file_cancelable(
        &self,
        cancel: CancellationToken,
        path: PathBuf,
        text: String,
    ) -> Result<()> {
        match &self.inner {
            RouterMode::InProcess(router) => {
                router.update_file_cancelable(cancel, path, text).await
            }
            RouterMode::Distributed(router) => {
                router.update_file_cancelable(cancel, path, text).await
            }
        }
    }

    pub async fn update_file(&self, path: PathBuf, text: String) -> Result<()> {
        self.update_file_cancelable(CancellationToken::new(), path, text)
            .await
    }

    pub async fn worker_stats(&self) -> Result<HashMap<ShardId, WorkerStats>> {
        match &self.inner {
            RouterMode::InProcess(router) => Ok(router.worker_stats()),
            RouterMode::Distributed(router) => router.worker_stats().await,
        }
    }

    pub async fn shutdown(&self) -> Result<()> {
        match &self.inner {
            RouterMode::InProcess(_) => Ok(()),
            RouterMode::Distributed(router) => router.shutdown().await,
        }
    }

    pub async fn workspace_symbols(&self, query: &str) -> Vec<Symbol> {
        match &self.inner {
            RouterMode::InProcess(router) => router.workspace_symbols(query).await,
            RouterMode::Distributed(router) => router.workspace_symbols(query).await,
        }
    }

    /// Best-effort diagnostics for a single file when running in distributed mode.
    ///
    /// This is intentionally minimal: it exists to enable an end-to-end distributed analysis
    /// prototype. Callers should treat failures as non-fatal.
    pub async fn diagnostics_cancelable(
        &self,
        cancel: CancellationToken,
        path: PathBuf,
    ) -> Vec<RemoteDiagnostic> {
        match &self.inner {
            RouterMode::InProcess(_) => Vec::new(),
            RouterMode::Distributed(router) => router.diagnostics_cancelable(cancel, path).await,
        }
    }

    pub async fn diagnostics(&self, path: PathBuf) -> Vec<RemoteDiagnostic> {
        self.diagnostics_cancelable(CancellationToken::new(), path)
            .await
    }
}

struct InProcessRouter {
    layout: WorkspaceLayout,
    global_revision: AtomicU64,
    shard_indexes: Mutex<HashMap<ShardId, ShardIndex>>,
    global_symbols: RwLock<GlobalSymbolIndex>,
    scheduler: Scheduler,
    index_token: Mutex<CancellationToken>,
}

impl InProcessRouter {
    fn new(layout: WorkspaceLayout) -> Self {
        let scheduler = tokio::runtime::Handle::try_current()
            .map(|handle| Scheduler::new_with_io_handle(SchedulerConfig::default(), handle))
            .unwrap_or_else(|_| Scheduler::default());
        Self {
            layout,
            global_revision: AtomicU64::new(0),
            shard_indexes: Mutex::new(HashMap::new()),
            global_symbols: RwLock::new(GlobalSymbolIndex::default()),
            scheduler,
            index_token: Mutex::new(CancellationToken::new()),
        }
    }

    async fn next_index_token(&self) -> CancellationToken {
        let mut guard = self.index_token.lock().await;
        guard.cancel();
        let token = CancellationToken::new();
        *guard = token.clone();
        token
    }

    async fn index_workspace(&self) -> Result<()> {
        self.index_workspace_cancelable(CancellationToken::new())
            .await
    }

    async fn index_workspace_cancelable(&self, cancel: CancellationToken) -> Result<()> {
        if cancel.is_cancelled() {
            return Err(rpc_cancelled_error());
        }

        let token = self.next_index_token().await;
        let revision = self.global_revision.fetch_add(1, Ordering::SeqCst) + 1;
        let mut collect_set = JoinSet::new();

        // Collect shard files concurrently so multi-shard indexing can start promptly.
        for (shard_id, root) in self.layout.source_roots.iter().enumerate() {
            let root = root.path.clone();
            let shard_id = shard_id as ShardId;
            collect_set.spawn(async move {
                let files = collect_java_file_paths(&root).await.with_context(|| {
                    format!("collect files for shard {shard_id} ({})", root.display())
                })?;
                Ok::<_, anyhow::Error>((shard_id, files))
            });
        }

        let mut indexes = HashMap::new();
        let mut join_set = JoinSet::new();

        // Pipeline file collection -> indexing so that early shards can start indexing work while
        // later shards are still walking the filesystem.
        while !collect_set.is_empty() || !join_set.is_empty() {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    token.cancel();
                    return Err(rpc_cancelled_error());
                }
                _ = token.cancelled() => {
                    return Ok(());
                }
                res = collect_set.join_next(), if !collect_set.is_empty() => {
                    let Some(res) = res else {
                        continue;
                    };

                    let (shard_id, files) = match res {
                        Ok(Ok(res)) => res,
                        Ok(Err(err)) => {
                            token.cancel();
                            return Err(err);
                        }
                        Err(err) => {
                            token.cancel();
                            return Err(anyhow!("file collection task panicked: {err}"));
                        }
                    };

                    // Spawn indexing for this shard immediately.
                    let task = self.scheduler.spawn_background_with_token(token.clone(), move |token| {
                        Cancelled::check(&token)?;
                        let symbols = index_for_files(shard_id, files, None, &token)?;
                        Cancelled::check(&token)?;
                        Ok(symbols)
                    });

                    join_set.spawn(async move { (shard_id, task.join().await) });
                }
                res = join_set.join_next(), if !join_set.is_empty() => {
                    let Some(res) = res else {
                        continue;
                    };

                    let (shard_id, symbols) = match res {
                        Ok((shard_id, res)) => (shard_id, res),
                        Err(err) => {
                            // The join task itself should never panic, but surface it as an indexing error.
                            token.cancel();
                            return Err(anyhow!("indexing task panicked: {err}"));
                        }
                    };

                    let symbols = match symbols {
                        Ok(symbols) => symbols,
                        Err(TaskError::Cancelled) => return Ok(()),
                        Err(TaskError::Panicked) => {
                            token.cancel();
                            return Err(anyhow!("indexing task panicked"));
                        }
                        Err(TaskError::DeadlineExceeded(_)) => {
                            token.cancel();
                            return Err(anyhow!("indexing task exceeded deadline"));
                        }
                    };
                    let symbols = match symbols {
                        Ok(symbols) => symbols,
                        Err(err) => {
                            token.cancel();
                            return Err(err);
                        }
                    };

                    indexes.insert(
                        shard_id,
                        ShardIndex {
                            shard_id,
                            revision,
                            index_generation: revision,
                            symbols,
                        },
                    );
                }
            }
        }

        if token.is_cancelled() {
            return Ok(());
        }
        if cancel.is_cancelled() {
            return Err(rpc_cancelled_error());
        }

        let symbols = build_global_symbols(indexes.values());

        // Check cancellation as close to committing as possible so a new indexing run can prevent
        // stale results from being installed (including during `build_global_symbols`).
        if token.is_cancelled() {
            return Ok(());
        }
        if cancel.is_cancelled() {
            return Err(rpc_cancelled_error());
        }

        {
            let mut guard = self.shard_indexes.lock().await;
            *guard = indexes;
        }

        write_global_symbols(&self.global_symbols, symbols, revision).await;
        Ok(())
    }

    async fn update_file(&self, path: PathBuf, text: String) -> Result<()> {
        self.update_file_cancelable(CancellationToken::new(), path, text)
            .await
    }

    async fn update_file_cancelable(
        &self,
        cancel: CancellationToken,
        path: PathBuf,
        text: String,
    ) -> Result<()> {
        if cancel.is_cancelled() {
            return Err(rpc_cancelled_error());
        }

        let token = self.next_index_token().await;
        let shard_id = self
            .layout
            .source_roots
            .iter()
            .enumerate()
            .find_map(|(id, root)| path.starts_with(&root.path).then_some(id as ShardId))
            .ok_or_else(|| anyhow!("file {path:?} not in any source root"))?;

        let revision = self.global_revision.fetch_add(1, Ordering::SeqCst) + 1;

        let mut shard_files =
            collect_java_file_paths(&self.layout.source_roots[shard_id as usize].path).await?;
        if cancel.is_cancelled() {
            token.cancel();
            return Err(rpc_cancelled_error());
        }
        let path_str = path.to_string_lossy().to_string();
        if !shard_files.iter().any(|file| file == &path_str) {
            shard_files.push(path_str.clone());
        }

        let task = self
            .scheduler
            .spawn_background_with_token(token.clone(), move |token| {
                Cancelled::check(&token)?;
                let symbols = index_for_files(shard_id, shard_files, Some((path_str, text)), &token)?;
                Cancelled::check(&token)?;
                Ok(symbols)
            });

        let join_result = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                token.cancel();
                return Err(rpc_cancelled_error());
            }
            res = task.join() => res,
        };

        let symbols = match join_result {
            Ok(symbols) => symbols,
            Err(TaskError::Cancelled) => return Ok(()),
            Err(TaskError::Panicked) => return Err(anyhow!("indexing task panicked")),
            Err(TaskError::DeadlineExceeded(_)) => {
                return Err(anyhow!("indexing task exceeded deadline"))
            }
        };
        let symbols = symbols?;

        if token.is_cancelled() {
            return Ok(());
        }
        if cancel.is_cancelled() {
            return Err(rpc_cancelled_error());
        }
        let new_index = ShardIndex {
            shard_id,
            revision,
            index_generation: revision,
            symbols,
        };

        let indexes_snapshot = {
            let mut guard = self.shard_indexes.lock().await;
            guard.insert(shard_id, new_index);
            guard.clone()
        };

        let symbols = build_global_symbols(indexes_snapshot.values());
        write_global_symbols(&self.global_symbols, symbols, revision).await;
        Ok(())
    }

    fn worker_stats(&self) -> HashMap<ShardId, WorkerStats> {
        HashMap::new()
    }

    async fn workspace_symbols(&self, query: &str) -> Vec<Symbol> {
        let guard = self.global_symbols.read().await;
        guard.search(query, WORKSPACE_SYMBOL_LIMIT)
    }
}

struct DistributedRouter {
    state: Arc<RouterState>,
    accept_task: Mutex<Option<JoinHandle<()>>>,
    worker_supervisors: Mutex<Vec<JoinHandle<()>>>,
    shutdown_tx: watch::Sender<bool>,
    bound_listen_addr_rx: watch::Receiver<Option<ListenAddr>>,
}

struct RouterState {
    config: DistributedRouterConfig,
    layout: WorkspaceLayout,
    next_worker_id: AtomicU32,
    global_revision: AtomicU64,
    shards: Mutex<HashMap<ShardId, ShardState>>,
    shard_indexes: Mutex<HashMap<ShardId, ShardIndex>>,
    shard_indexes_update_id: AtomicU64,
    global_symbols: RwLock<GlobalSymbolIndex>,
    notify: Notify,
    handshake_semaphore: Arc<Semaphore>,
    connection_semaphore: Arc<Semaphore>,
    bound_listen_addr_tx: watch::Sender<Option<ListenAddr>>,
}

struct ShardState {
    root: PathBuf,
    worker: Option<WorkerHandle>,
    pending_worker: Option<WorkerId>,
}

#[derive(Clone)]
struct WorkerHandle {
    shard_id: ShardId,
    worker_id: WorkerId,
    conn: RpcConnection,
}

impl DistributedRouter {
    async fn new(config: DistributedRouterConfig, layout: WorkspaceLayout) -> Result<Self> {
        let mut config = config;
        if config.spawn_workers && config.auth_token.is_none() {
            config.auth_token = Some(ipc_security::generate_auth_token()?);
        }

        config.validate()?;
        #[cfg(feature = "tls")]
        config
            .tls_client_cert_fingerprint_allowlist
            .normalize_in_place()?;
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (bound_listen_addr_tx, bound_listen_addr_rx) = watch::channel(None);

        let handshake_limit = config.max_inflight_handshakes.max(1);
        let connection_limit = config.max_worker_connections.max(1);
        let handshake_semaphore = Arc::new(Semaphore::new(handshake_limit));
        let connection_semaphore = Arc::new(Semaphore::new(connection_limit));

        info!(
            listen_addr = ?config.listen_addr,
            spawn_workers = config.spawn_workers,
            cache_dir = %config.cache_dir.display(),
            worker_command = %config.worker_command.display(),
            "starting distributed router"
        );
        let mut shards = HashMap::new();
        for (idx, root) in layout.source_roots.iter().enumerate() {
            shards.insert(
                idx as ShardId,
                ShardState {
                    root: root.path.clone(),
                    worker: None,
                    pending_worker: None,
                },
            );
        }

        let state = Arc::new(RouterState {
            config: config.clone(),
            layout,
            next_worker_id: AtomicU32::new(1),
            global_revision: AtomicU64::new(0),
            shards: Mutex::new(shards),
            shard_indexes: Mutex::new(HashMap::new()),
            shard_indexes_update_id: AtomicU64::new(0),
            global_symbols: RwLock::new(GlobalSymbolIndex::default()),
            notify: Notify::new(),
            handshake_semaphore,
            connection_semaphore,
            bound_listen_addr_tx,
        });

        let accept_state = state.clone();
        let accept_shutdown_rx = shutdown_rx.clone();
        let accept_task = tokio::spawn(async move {
            let listen_addr = accept_state.config.listen_addr.clone();
            if let Err(err) = accept_loop(accept_state, accept_shutdown_rx).await {
                error!(
                    listen_addr = ?listen_addr,
                    error = ?err,
                    "router accept loop terminated"
                );
            }
        });

        let mut worker_supervisors = Vec::new();
        if config.spawn_workers {
            for shard_id in 0..(state.layout.source_roots.len() as ShardId) {
                let supervisor_state = state.clone();
                let mut supervisor_shutdown_rx = shutdown_rx.clone();
                worker_supervisors.push(tokio::spawn(async move {
                    worker_supervisor_loop(supervisor_state, shard_id, &mut supervisor_shutdown_rx)
                        .await;
                }));
            }
        }

        Ok(Self {
            state,
            accept_task: Mutex::new(Some(accept_task)),
            worker_supervisors: Mutex::new(worker_supervisors),
            shutdown_tx,
            bound_listen_addr_rx,
        })
    }

    async fn bound_listen_addr(&self) -> Option<ListenAddr> {
        let mut rx = self.bound_listen_addr_rx.clone();
        loop {
            if let Some(addr) = rx.borrow().clone() {
                return Some(addr);
            }

            if rx.changed().await.is_err() {
                return None;
            }
        }
    }

    async fn index_workspace(&self) -> Result<()> {
        self.index_workspace_cancelable(CancellationToken::new())
            .await
    }

    async fn index_workspace_cancelable(&self, cancel: CancellationToken) -> Result<()> {
        if cancel.is_cancelled() {
            return Err(rpc_cancelled_error());
        }

        let revision = self.state.global_revision.fetch_add(1, Ordering::SeqCst) + 1;
        if self.state.layout.source_roots.is_empty() {
            {
                let mut guard = self.state.shard_indexes.lock().await;
                guard.clear();
            }
            let update_id = self
                .state
                .shard_indexes_update_id
                .fetch_add(1, Ordering::SeqCst)
                + 1;
            write_global_symbols(&self.state.global_symbols, Vec::new(), update_id).await;
            return Ok(());
        }

        let mut join_set = JoinSet::new();
        let snapshot_semaphore = Arc::new(Semaphore::new(
            MAX_CONCURRENT_SHARD_FILE_SNAPSHOTS.max(1),
        ));
        for shard_id in 0..(self.state.layout.source_roots.len() as ShardId) {
            let state = self.state.clone();
            let root = self.state.layout.source_roots[shard_id as usize]
                .path
                .clone();
            let cancel = cancel.clone();
            let snapshot_semaphore = Arc::clone(&snapshot_semaphore);

            join_set.spawn(async move {
                if cancel.is_cancelled() {
                    return Err(rpc_cancelled_error());
                }

                let worker = wait_for_worker_cancelable(state.clone(), shard_id, &cancel)
                    .await
                    .with_context(|| format!("wait for worker for shard {shard_id}"))?;

                if cancel.is_cancelled() {
                    return Err(rpc_cancelled_error());
                }

                // Limit how many shards can build + send full file snapshots concurrently.
                let snapshot_permit: OwnedSemaphorePermit = snapshot_semaphore
                    .acquire_owned()
                    .await
                    .map_err(|_| anyhow!("file snapshot semaphore closed"))?;

                let files = collect_java_files(&root).await.with_context(|| {
                    format!("collect files for shard {shard_id} ({})", root.display())
                })?;

                if cancel.is_cancelled() {
                    return Err(rpc_cancelled_error());
                }

                // Start the RPC call (which serializes/writes the full snapshot) while holding the
                // snapshot permit, then drop it before waiting for the worker to finish indexing so
                // the next shard can begin snapshotting.
                let pending: PendingCall = match timeout(
                    WORKER_RPC_WRITE_TIMEOUT,
                    worker.conn.start_call(Request::IndexShard { revision, files }),
                )
                .await
                {
                    Ok(Ok(pending)) => pending,
                    Ok(Err(err)) => {
                        return Err(anyhow!(err)).with_context(|| {
                            format!(
                                "send request to worker {} (shard {})",
                                worker.worker_id, worker.shard_id
                            )
                        });
                    }
                    Err(_) => {
                        let _ = worker.conn.shutdown().await;
                        return Err(anyhow!(
                            "timed out writing request to worker {} (shard {})",
                            worker.worker_id,
                            worker.shard_id
                        ));
                    }
                };

                drop(snapshot_permit);

                let request_id = pending.request_id();

                let resp = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        // Best-effort: cancellation is advisory and the caller may already have moved on.
                        let _ = timeout(WORKER_RPC_CANCEL_TIMEOUT, worker.conn.cancel(request_id)).await;
                        return Err(rpc_cancelled_error());
                    }
                    res = timeout(WORKER_RPC_READ_TIMEOUT, pending.wait()) => {
                        match res {
                            Ok(Ok(resp)) => resp,
                            Ok(Err(err)) => match err {
                                nova_remote_rpc::RpcError::Canceled => return Err(rpc_cancelled_error()),
                                err => {
                                    return Err(anyhow!(err)).with_context(|| {
                                        format!(
                                            "receive response from worker {} (shard {})",
                                            worker.worker_id, worker.shard_id
                                        )
                                    })
                                }
                            },
                            Err(_) => {
                                let _ = worker.conn.shutdown().await;
                                return Err(anyhow!(
                                    "timed out waiting for response from worker {} (shard {})",
                                    worker.worker_id,
                                    worker.shard_id
                                ));
                            }
                        }
                    }
                };

                Ok::<_, anyhow::Error>((shard_id, worker, resp))
            });
        }

        let mut updated_any = false;
        let mut error: Option<anyhow::Error> = None;
        let mut cancelled = false;

        while !join_set.is_empty() {
            let res = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    cancelled = true;
                    break;
                }
                res = join_set.join_next() => res,
            };

            let Some(res) = res else {
                break;
            };

            let (shard_id, worker, resp) = match res {
                Ok(Ok(res)) => res,
                Ok(Err(err)) => {
                    error = Some(err);
                    break;
                }
                Err(err) => {
                    error = Some(anyhow!("indexing task panicked: {err}"));
                    break;
                }
            };

            match resp {
                Response::ShardIndex(index) => {
                    if index.shard_id != shard_id {
                        self.disconnect_worker(&worker).await;
                        error = Some(anyhow!(
                            "worker returned index for wrong shard {} (expected {shard_id})",
                            index.shard_id
                        ));
                        break;
                    }

                    // Apply the shard index immediately, but defer rebuilding the global symbol
                    // index until the end to avoid quadratic rebuild work.
                    let applied = {
                        let incoming_key = (index.revision, index.index_generation);
                        let mut guard = self.state.shard_indexes.lock().await;
                        if let Some(current) = guard.get(&shard_id) {
                            let current_key = (current.revision, current.index_generation);
                            if incoming_key < current_key {
                                false
                            } else {
                                guard.insert(shard_id, index);
                                self.state
                                    .shard_indexes_update_id
                                    .fetch_add(1, Ordering::SeqCst);
                                true
                            }
                        } else {
                            guard.insert(shard_id, index);
                            self.state
                                .shard_indexes_update_id
                                .fetch_add(1, Ordering::SeqCst);
                            true
                        }
                    };
                    updated_any |= applied;
                }
                other => {
                    error = Some(anyhow!("unexpected worker response: {other:?}"));
                    break;
                }
            }
        }

        if cancelled {
            // Detach the in-flight tasks so they can observe the cancellation token and (if a
            // request was already started) best-effort send v3 Cancel packets to workers.
            tokio::spawn(async move { while join_set.join_next().await.is_some() {} });
            return Err(rpc_cancelled_error());
        }

        // If anything went wrong mid-flight, abort remaining RPC tasks.
        drop(join_set);

        // Keep `global_symbols` consistent with `shard_indexes` while avoiding quadratic rebuilds:
        // rebuild once from a full snapshot at the end (even if we return early on error after
        // applying some shard indexes).
        if updated_any {
            let (mut symbols, update_id) = {
                let guard = self.state.shard_indexes.lock().await;
                let update_id = self.state.shard_indexes_update_id.load(Ordering::SeqCst);
                let mut symbols = Vec::new();
                for shard in guard.values() {
                    symbols.extend(shard.symbols.iter().cloned());
                }
                (symbols, update_id)
            };
            symbols.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.path.cmp(&b.path)));
            symbols.dedup();
            write_global_symbols(&self.state.global_symbols, symbols, update_id).await;
        }

        if let Some(err) = error {
            return Err(err);
        }

        Ok(())
    }

    async fn update_file(&self, path: PathBuf, text: String) -> Result<()> {
        self.update_file_cancelable(CancellationToken::new(), path, text)
            .await
    }

    async fn update_file_cancelable(
        &self,
        cancel: CancellationToken,
        path: PathBuf,
        text: String,
    ) -> Result<()> {
        if cancel.is_cancelled() {
            return Err(rpc_cancelled_error());
        }

        let shard_id = self
            .state
            .layout
            .source_roots
            .iter()
            .enumerate()
            .find_map(|(id, root)| path.starts_with(&root.path).then_some(id as ShardId))
            .ok_or_else(|| anyhow!("file {path:?} not in any source root"))?;

        let revision = self.state.global_revision.fetch_add(1, Ordering::SeqCst) + 1;
        let worker = wait_for_worker_cancelable(self.state.clone(), shard_id, &cancel).await?;
        let file = FileText {
            path: path.to_string_lossy().to_string(),
            text,
        };

        let resp = worker_call_cancelable(&worker, &cancel, Request::UpdateFile { revision, file })
            .await?;
        match resp {
            Response::ShardIndex(index) => {
                if index.shard_id != shard_id {
                    self.disconnect_worker(&worker).await;
                    return Err(anyhow!(
                        "worker returned index for wrong shard {} (expected {shard_id})",
                        index.shard_id
                    ));
                }
                apply_shard_index(self.state.clone(), index).await;
                Ok(())
            }
            other => Err(anyhow!("unexpected worker response: {other:?}")),
        }
    }

    async fn worker_stats(&self) -> Result<HashMap<ShardId, WorkerStats>> {
        let shard_ids: Vec<ShardId> =
            (0..self.state.layout.source_roots.len() as ShardId).collect();
        let mut stats = HashMap::new();
        for shard_id in shard_ids {
            let worker = wait_for_worker(self.state.clone(), shard_id).await?;
            let resp = worker_call(&worker, Request::GetWorkerStats).await?;
            match resp {
                Response::WorkerStats(ws) => {
                    if ws.shard_id != worker.shard_id {
                        self.disconnect_worker(&worker).await;
                        return Err(anyhow!(
                            "worker {} returned stats for wrong shard {} (expected {shard_id})",
                            worker.worker_id,
                            ws.shard_id
                        ));
                    }
                    stats.insert(shard_id, ws);
                }
                other => return Err(anyhow!("unexpected worker response: {other:?}")),
            }
        }
        Ok(stats)
    }

    async fn workspace_symbols(&self, query: &str) -> Vec<Symbol> {
        let guard = self.state.global_symbols.read().await;
        guard.search(query, WORKSPACE_SYMBOL_LIMIT)
    }

    async fn diagnostics(&self, path: PathBuf) -> Vec<RemoteDiagnostic> {
        self.diagnostics_cancelable(CancellationToken::new(), path)
            .await
    }

    async fn diagnostics_cancelable(
        &self,
        cancel: CancellationToken,
        path: PathBuf,
    ) -> Vec<RemoteDiagnostic> {
        if cancel.is_cancelled() {
            return Vec::new();
        }

        let shard_id = self
            .state
            .layout
            .source_roots
            .iter()
            .enumerate()
            .find_map(|(id, root)| path.starts_with(&root.path).then_some(id as ShardId));

        let Some(shard_id) = shard_id else {
            return Vec::new();
        };

        let worker = match wait_for_worker_cancelable(self.state.clone(), shard_id, &cancel).await {
            Ok(worker) => worker,
            Err(err) => {
                if err
                    .downcast_ref::<nova_remote_rpc::RpcError>()
                    .is_some_and(|err| matches!(err, nova_remote_rpc::RpcError::Canceled))
                {
                    return Vec::new();
                }
                warn!(
                    shard_id,
                    error = ?err,
                    "diagnostics request dropped: shard worker unavailable"
                );
                return Vec::new();
            }
        };

        let worker_id = worker.worker_id;
        let path_str = path.to_string_lossy().to_string();
        match worker_call_cancelable(&worker, &cancel, Request::Diagnostics { path: path_str })
            .await
        {
            Ok(Response::Diagnostics { diagnostics }) => diagnostics,
            Ok(other) => {
                warn!(
                    shard_id,
                    worker_id,
                    response = ?other,
                    "unexpected worker response for diagnostics request"
                );
                Vec::new()
            }
            Err(err) => {
                if err
                    .downcast_ref::<nova_remote_rpc::RpcError>()
                    .is_some_and(|err| matches!(err, nova_remote_rpc::RpcError::Canceled))
                {
                    return Vec::new();
                }
                warn!(
                    shard_id,
                    worker_id,
                    error = ?err,
                    "diagnostics request failed"
                );
                Vec::new()
            }
        }
    }

    async fn shutdown(&self) -> Result<()> {
        let _ = self.shutdown_tx.send(true);

        let worker_conns: Vec<RpcConnection> = {
            let guard = self.state.shards.lock().await;
            guard
                .values()
                .filter_map(|s| s.worker.as_ref().map(|w| w.conn.clone()))
                .collect()
        };

        let mut shutdown_tasks = Vec::new();
        for conn in worker_conns {
            shutdown_tasks.push(tokio::spawn(async move {
                // Best-effort: ask the worker to shut down and wait for the response so the
                // request has definitely made it onto the wire before we close the transport.
                //
                // Closing immediately after `start_call()` is racy: the write loop may observe
                // the shutdown signal first and drop queued frames, leaving workers hung until
                // their own watchdogs/firewalls kick in.
                if let Ok(Ok(pending)) =
                    timeout(WORKER_RPC_WRITE_TIMEOUT, conn.start_call(Request::Shutdown)).await
                {
                    let _ = timeout(WORKER_SHUTDOWN_RPC_TIMEOUT, pending.wait()).await;
                }

                let _ = conn.shutdown().await;
            }));
        }

        // Drive the tasks to completion before returning so external processes (e.g. test
        // fixtures) can observe a bounded shutdown.
        for task in shutdown_tasks {
            let _ = task.await;
        }

        tokio::time::sleep(Duration::from_millis(50)).await;

        if let Some(mut task) = self.accept_task.lock().await.take() {
            if timeout(Duration::from_secs(1), &mut task).await.is_err() {
                task.abort();
                let _ = task.await;
            }
        }

        let tasks = std::mem::take(&mut *self.worker_supervisors.lock().await);
        for mut task in tasks {
            if timeout(Duration::from_secs(1), &mut task).await.is_err() {
                task.abort();
                let _ = task.await;
            }
        }

        #[cfg(unix)]
        if let ListenAddr::Unix(path) = &self.state.config.listen_addr {
            let _ = std::fs::remove_file(path);
        }

        Ok(())
    }

    async fn disconnect_worker(&self, worker: &WorkerHandle) {
        // Treat shard mismatches as a protocol violation and sever the connection so it cannot
        // keep returning poisoned cross-shard responses.
        let _ = worker.conn.shutdown().await;

        let mut guard = self.state.shards.lock().await;
        if let Some(shard) = guard.get_mut(&worker.shard_id) {
            if shard
                .worker
                .as_ref()
                .is_some_and(|w| w.worker_id == worker.worker_id)
            {
                shard.worker = None;
            }
            if shard.pending_worker == Some(worker.worker_id) {
                shard.pending_worker = None;
            }
        }
        drop(guard);
        self.state.notify.notify_waiters();
    }
}

async fn wait_for_worker(state: Arc<RouterState>, shard_id: ShardId) -> Result<WorkerHandle> {
    let cancel = CancellationToken::new();
    wait_for_worker_cancelable(state, shard_id, &cancel).await
}

async fn wait_for_worker_cancelable(
    state: Arc<RouterState>,
    shard_id: ShardId,
    cancel: &CancellationToken,
) -> Result<WorkerHandle> {
    if cancel.is_cancelled() {
        return Err(rpc_cancelled_error());
    }

    timeout(WORKER_WAIT_TIMEOUT, async {
        loop {
            if cancel.is_cancelled() {
                return Err(rpc_cancelled_error());
            }

            if let Some(worker) = {
                let guard = state.shards.lock().await;
                guard.get(&shard_id).and_then(|s| s.worker.clone())
            } {
                if worker.shard_id != shard_id {
                    return Err(anyhow!(
                        "internal error: shard {shard_id} mapped to worker for shard {}",
                        worker.shard_id
                    ));
                }
                return Ok(worker);
            }

            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(rpc_cancelled_error()),
                _ = state.notify.notified() => {}
            }
        }
    })
    .await
    .context("timed out waiting for worker")?
}

async fn apply_shard_index(state: Arc<RouterState>, index: ShardIndex) {
    let (indexes_snapshot, update_id) = {
        let mut guard = state.shard_indexes.lock().await;
        if let Some(current) = guard.get(&index.shard_id) {
            let current_key = (current.revision, current.index_generation);
            let incoming_key = (index.revision, index.index_generation);
            if incoming_key < current_key {
                return;
            }
        }
        guard.insert(index.shard_id, index);
        let update_id = state.shard_indexes_update_id.fetch_add(1, Ordering::SeqCst) + 1;
        (guard.clone(), update_id)
    };

    let symbols = build_global_symbols(indexes_snapshot.values());
    write_global_symbols(&state.global_symbols, symbols, update_id).await;
}

async fn worker_call(worker: &WorkerHandle, request: Request) -> Result<Response> {
    let cancel = CancellationToken::new();
    worker_call_cancelable(worker, &cancel, request).await
}

async fn worker_call_cancelable(
    worker: &WorkerHandle,
    cancel: &CancellationToken,
    request: Request,
) -> Result<Response> {
    if cancel.is_cancelled() {
        return Err(rpc_cancelled_error());
    }

    let pending: PendingCall =
        match timeout(WORKER_RPC_WRITE_TIMEOUT, worker.conn.start_call(request)).await {
            Ok(Ok(pending)) => pending,
            Ok(Err(err)) => {
                return Err(anyhow!(err)).with_context(|| {
                    format!(
                        "send request to worker {} (shard {})",
                        worker.worker_id, worker.shard_id
                    )
                })
            }
            Err(_) => {
                let _ = worker.conn.shutdown().await;
                return Err(anyhow!(
                    "timed out writing request to worker {} (shard {})",
                    worker.worker_id,
                    worker.shard_id
                ));
            }
        };

    let request_id = pending.request_id();

    tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            // Best-effort: cancellation is advisory and the caller may already have moved on.
            let _ = timeout(WORKER_RPC_CANCEL_TIMEOUT, worker.conn.cancel(request_id)).await;
            Err(rpc_cancelled_error())
        }
        res = timeout(WORKER_RPC_READ_TIMEOUT, pending.wait()) => {
            match res {
                Ok(Ok(resp)) => Ok(resp),
                Ok(Err(err)) => match err {
                    nova_remote_rpc::RpcError::Canceled => Err(rpc_cancelled_error()),
                    err => Err(anyhow!(err)).with_context(|| {
                        format!(
                            "receive response from worker {} (shard {})",
                            worker.worker_id, worker.shard_id
                        )
                    }),
                },
                Err(_) => {
                    let _ = worker.conn.shutdown().await;
                    Err(anyhow!(
                        "timed out waiting for response from worker {} (shard {})",
                        worker.worker_id,
                        worker.shard_id
                    ))
                }
            }
        }
    }
}

async fn accept_loop(
    state: Arc<RouterState>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    let listen_addr = state.config.listen_addr.clone();
    match listen_addr {
        #[cfg(unix)]
        ListenAddr::Unix(path) => accept_loop_unix(state, path, &mut shutdown_rx).await,
        #[cfg(windows)]
        ListenAddr::NamedPipe(name) => accept_loop_named_pipe(state, name, &mut shutdown_rx).await,
        ListenAddr::Tcp(cfg) => accept_loop_tcp(state, cfg, &mut shutdown_rx).await,
    }
}

#[cfg(unix)]
async fn accept_loop_unix(
    state: Arc<RouterState>,
    path: PathBuf,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> Result<()> {
    let _ = std::fs::remove_file(&path);
    ipc_security::ensure_unix_socket_dir(&path)?;

    let listener =
        UnixListener::bind(&path).with_context(|| format!("bind unix socket {path:?}"))?;
    ipc_security::restrict_unix_socket_permissions(&path)?;
    let _ = state
        .bound_listen_addr_tx
        .send(Some(ListenAddr::Unix(path.clone())));

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    return Ok(());
                }
            }
            res = listener.accept() => {
                let (stream, _) = res.with_context(|| format!("accept unix socket {path:?}"))?;
                if state.config.auth_token.is_none() {
                    match ipc_security::unix_peer_uid_matches_current_user(&stream) {
                        Ok(true) => {}
                        Ok(false) => {
                            warn!(
                                socket_path = %path.display(),
                                "rejecting unix socket connection from different uid"
                            );
                            continue;
                        }
                        Err(err) => {
                            warn!(
                                socket_path = %path.display(),
                                error = ?err,
                                "failed to read unix peer credentials"
                            );
                            continue;
                        }
                    }
                }
                let boxed: BoxedStream = Box::new(stream);
                let Ok(connection_permit) =
                    state.connection_semaphore.clone().try_acquire_owned()
                else {
                    warn!(
                        socket_path = %path.display(),
                        "dropping incoming unix connection: too many active connections"
                    );
                    continue;
                };
                let Ok(handshake_permit) = state.handshake_semaphore.clone().try_acquire_owned() else {
                    warn!(
                        socket_path = %path.display(),
                        "dropping incoming unix connection: too many pending handshakes"
                    );
                    continue;
                };
                let conn_state = state.clone();
                let socket_path = path.clone();
                tokio::spawn(async move {
                    let _handshake_permit = handshake_permit;
                    if let Err(err) =
                        handle_new_connection(
                            conn_state,
                            boxed,
                            WorkerIdentity::Unauthenticated,
                            connection_permit,
                        )
                            .await
                    {
                        warn!(
                            socket_path = %socket_path.display(),
                            error = ?err,
                            "failed to handle worker connection"
                        );
                    }
                });
            }
        }
    }
}

#[cfg(windows)]
async fn accept_loop_named_pipe(
    state: Arc<RouterState>,
    name: String,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> Result<()> {
    let name = normalize_pipe_name(&name);
    let mut server = ipc_security::create_secure_named_pipe_server(&name, true)
        .with_context(|| format!("create named pipe {name}"))?;
    let _ = state
        .bound_listen_addr_tx
        .send(Some(ListenAddr::NamedPipe(name.clone())));

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    return Ok(());
                }
            }
            res = server.connect() => {
                res.with_context(|| format!("accept named pipe {name}"))?;
                let stream: BoxedStream = Box::new(server);
                server = ipc_security::create_secure_named_pipe_server(&name, false)
                    .with_context(|| format!("create named pipe {name}"))?;
                let Ok(connection_permit) =
                    state.connection_semaphore.clone().try_acquire_owned()
                else {
                    warn!(
                        pipe_name = %name,
                        "dropping incoming named pipe connection: too many active connections"
                    );
                    continue;
                };
                let Ok(handshake_permit) = state.handshake_semaphore.clone().try_acquire_owned() else {
                    warn!(
                        pipe_name = %name,
                        "dropping incoming named pipe connection: too many pending handshakes"
                    );
                    continue;
                };
                let conn_state = state.clone();
                let pipe_name = name.clone();
                tokio::spawn(async move {
                    let _handshake_permit = handshake_permit;
                    if let Err(err) =
                        handle_new_connection(
                            conn_state,
                            stream,
                            WorkerIdentity::Unauthenticated,
                            connection_permit,
                        )
                            .await
                    {
                        warn!(
                            pipe_name = %pipe_name,
                            error = ?err,
                            "failed to handle worker connection"
                        );
                    }
                });
            }
        }
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

async fn accept_loop_tcp(
    state: Arc<RouterState>,
    cfg: TcpListenAddr,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> Result<()> {
    let addr = match &cfg {
        TcpListenAddr::Plain(addr) => *addr,
        #[cfg(feature = "tls")]
        TcpListenAddr::Tls { addr, .. } => *addr,
    };
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind tcp listener {addr}"))?;
    let local_addr = listener.local_addr().context("tcp listener local_addr")?;
    let cfg = match cfg {
        TcpListenAddr::Plain(_) => TcpListenAddr::Plain(local_addr),
        #[cfg(feature = "tls")]
        TcpListenAddr::Tls { config, .. } => TcpListenAddr::Tls {
            addr: local_addr,
            config,
        },
    };
    let addr = local_addr;
    let _ = state
        .bound_listen_addr_tx
        .send(Some(ListenAddr::Tcp(cfg.clone())));

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    return Ok(());
                }
            }
            res = listener.accept() => {
                let (stream, peer_addr) = res.with_context(|| format!("accept tcp {addr}"))?;
                let Ok(connection_permit) =
                    state.connection_semaphore.clone().try_acquire_owned()
                else {
                    warn!(
                        peer_addr = %peer_addr,
                        "dropping incoming tcp connection: too many active connections"
                    );
                    continue;
                };
                let Ok(handshake_permit) = state.handshake_semaphore.clone().try_acquire_owned() else {
                    warn!(
                        peer_addr = %peer_addr,
                        "dropping incoming tcp connection: too many pending handshakes"
                    );
                    continue;
                };
                let conn_state = state.clone();
                let cfg = cfg.clone();
                tokio::spawn(async move {
                    let _handshake_permit = handshake_permit;
                    let (boxed, identity): (BoxedStream, WorkerIdentity) = match cfg {
                        TcpListenAddr::Plain(_) => (Box::new(stream), WorkerIdentity::Unauthenticated),
                        #[cfg(feature = "tls")]
                        TcpListenAddr::Tls { config, .. } => {
                            let accepted =
                                match timeout(WORKER_HANDSHAKE_TIMEOUT, tls::accept(stream, config))
                                    .await
                                {
                                    Ok(res) => res,
                                    Err(_) => {
                                        warn!(peer_addr = %peer_addr, "tls accept timed out");
                                        return;
                                    }
                                };
                            let accepted = match accepted {
                                Ok(accepted) => accepted,
                                Err(err) => {
                                    warn!(peer_addr = %peer_addr, error = ?err, "tls accept failed");
                                    return;
                                }
                            };
                            let identity = accepted
                                .client_cert_fingerprint
                                .map(WorkerIdentity::TlsClientCertFingerprint)
                                .unwrap_or(WorkerIdentity::Unauthenticated);
                            (Box::new(accepted.stream), identity)
                        }
                    };
                    let identity_for_log = identity.clone();
                    if let Err(err) =
                        handle_new_connection(conn_state, boxed, identity, connection_permit).await
                    {
                        warn!(
                            peer_addr = %peer_addr,
                            identity = ?identity_for_log,
                            error = ?err,
                            "failed to handle worker connection"
                        );
                    }
                });
            }
        }
    }
}

type BoxedStream = Box<dyn AsyncReadWrite>;

trait AsyncReadWrite: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T> AsyncReadWrite for T where T: AsyncRead + AsyncWrite + Unpin + Send {}

async fn handle_new_connection(
    state: Arc<RouterState>,
    stream: BoxedStream,
    identity: WorkerIdentity,
    connection_permit: OwnedSemaphorePermit,
) -> Result<()> {
    let max_rpc_bytes = state
        .config
        .max_rpc_bytes
        .min(nova_remote_proto::MAX_MESSAGE_BYTES)
        .max(1);
    let max_rpc_len: u32 = max_rpc_bytes.try_into().unwrap_or(u32::MAX);

    let mut cfg = RpcRouterConfig::default();
    cfg.expected_auth_token = state.config.auth_token.clone();
    cfg.pre_handshake_max_frame_len = MAX_HELLO_BYTES
        .try_into()
        .unwrap_or(nova_remote_rpc::DEFAULT_PRE_HANDSHAKE_MAX_FRAME_LEN);
    cfg.capabilities.max_frame_len = max_rpc_len;
    cfg.capabilities.max_packet_len = max_rpc_len;
    cfg.capabilities.supports_cancel = true;

    let reservation = Arc::new(tokio::sync::Mutex::new(None::<(ShardId, WorkerId)>));
    let reservation_hook = reservation.clone();

    let admission_state = state.clone();
    let admission_identity = identity.clone();
    #[cfg(not(feature = "tls"))]
    let _ = &admission_identity;

    let handshake = timeout(
        WORKER_HANDSHAKE_TIMEOUT,
        RpcConnection::handshake_as_router_with_config_and_admission(stream, cfg, move |hello| {
            let shard_id = hello.shard_id;
            let reservation_hook = reservation_hook.clone();
            let admission_state = admission_state.clone();
            let admission_identity = admission_identity.clone();
            #[cfg(not(feature = "tls"))]
            let _ = &admission_identity;
            async move {
                #[cfg(feature = "tls")]
                {
                    let allowlist = &admission_state.config.tls_client_cert_fingerprint_allowlist;
                    let shard_allowlist = allowlist.shards.get(&shard_id);
                    let enforce_allowlist =
                        !allowlist.global.is_empty() || shard_allowlist.is_some();

                    if enforce_allowlist {
                        let Some(fingerprint) = admission_identity.tls_client_cert_fingerprint()
                        else {
                            return RouterAdmission::Reject(HandshakeReject {
                                code: RejectCode::Unauthorized,
                                message: "shard authorization failed".into(),
                            });
                        };

                        let is_allowed = allowlist
                            .global
                            .iter()
                            .any(|allowed| allowed.eq_ignore_ascii_case(fingerprint))
                            || shard_allowlist.is_some_and(|entries| {
                                entries
                                    .iter()
                                    .any(|allowed| allowed.eq_ignore_ascii_case(fingerprint))
                            });

                        if !is_allowed {
                            return RouterAdmission::Reject(HandshakeReject {
                                code: RejectCode::Unauthorized,
                                message: "shard authorization failed".into(),
                            });
                        }
                    }
                }

                let mut guard = admission_state.shards.lock().await;
                let Some(shard) = guard.get_mut(&shard_id) else {
                    return RouterAdmission::Reject(HandshakeReject {
                        code: RejectCode::InvalidRequest,
                        message: format!("unknown shard {shard_id}"),
                    });
                };

                if shard.worker.is_some() || shard.pending_worker.is_some() {
                    return RouterAdmission::Reject(HandshakeReject {
                        code: RejectCode::InvalidRequest,
                        message: format!("shard {shard_id} already has a connected worker"),
                    });
                }

                let worker_id: WorkerId = admission_state
                    .next_worker_id
                    .fetch_add(1, Ordering::SeqCst);
                shard.pending_worker = Some(worker_id);
                *reservation_hook.lock().await = Some((shard_id, worker_id));

                let revision = admission_state.global_revision.load(Ordering::SeqCst);
                RouterAdmission::Accept {
                    worker_id,
                    revision,
                }
            }
        }),
    )
    .await;

    let (conn, welcome, hello) = match handshake {
        Ok(Ok(res)) => res,
        Ok(Err(err)) => {
            if let Some((shard_id, worker_id)) = reservation.lock().await.take() {
                let mut guard = state.shards.lock().await;
                if let Some(shard) = guard.get_mut(&shard_id) {
                    if shard.pending_worker == Some(worker_id) {
                        shard.pending_worker = None;
                    }
                }
                state.notify.notify_waiters();
            }
            return Err(anyhow!(err));
        }
        Err(_) => {
            if let Some((shard_id, worker_id)) = reservation.lock().await.take() {
                let mut guard = state.shards.lock().await;
                if let Some(shard) = guard.get_mut(&shard_id) {
                    if shard.pending_worker == Some(worker_id) {
                        shard.pending_worker = None;
                    }
                }
                state.notify.notify_waiters();
            }
            return Err(anyhow!("timed out waiting for worker handshake"));
        }
    };

    let shard_id = welcome.shard_id;
    let worker_id = welcome.worker_id;
    let has_cached_index = hello.cached_index_info.is_some();

    let handle = WorkerHandle {
        shard_id,
        worker_id,
        conn: conn.clone(),
    };

    // Finalize the reservation now that the welcome frame is on the wire.
    {
        let mut guard = state.shards.lock().await;
        let Some(shard) = guard.get_mut(&shard_id) else {
            return Err(anyhow!(
                "BUG: shard {shard_id} disappeared during handshake"
            ));
        };

        if shard.pending_worker != Some(worker_id) {
            return Err(anyhow!(
                "BUG: shard {shard_id} pending worker mismatch during handshake"
            ));
        }
        shard.pending_worker = None;
        shard.worker = Some(handle.clone());
    }

    info!(shard_id, worker_id, has_cached_index, "worker connected");

    conn.set_notification_handler({
        let notif_state = state.clone();
        move |notification| {
            let notif_state = notif_state.clone();
            async move {
                match notification {
                    Notification::CachedIndex(index) => {
                        if index.shard_id != shard_id {
                            warn!(
                                shard_id,
                                worker_id,
                                reported_shard_id = index.shard_id,
                                "worker sent cached index for wrong shard; disconnecting worker"
                            );

                            // Remove the worker handle first so a replacement connection isn't
                            // blocked on the accept-loop shard reservation check.
                            let conn = {
                                let mut guard = notif_state.shards.lock().await;
                                let Some(shard) = guard.get_mut(&shard_id) else {
                                    return;
                                };
                                let Some(worker) = shard.worker.as_ref() else {
                                    return;
                                };
                                if worker.worker_id != worker_id {
                                    return;
                                }

                                let conn = worker.conn.clone();
                                shard.worker = None;
                                if shard.pending_worker == Some(worker_id) {
                                    shard.pending_worker = None;
                                }
                                conn
                            };
                            notif_state.notify.notify_waiters();

                            // Close the transport outside the mutex to avoid holding router state
                            // across an await.
                            let _ = conn.shutdown().await;
                            return;
                        }

                        apply_shard_index(notif_state, index).await;
                    }
                    Notification::Unknown => {}
                }
            }
        }
    });

    let cleanup_state = state.clone();
    let cleanup_conn = conn.clone();
    tokio::spawn(async move {
        let _connection_permit = connection_permit;
        let _ = cleanup_conn.wait_closed().await;
        info!(shard_id, worker_id, "worker connection closed");
        let mut guard = cleanup_state.shards.lock().await;
        if let Some(shard) = guard.get_mut(&shard_id) {
            if shard
                .worker
                .as_ref()
                .is_some_and(|w| w.worker_id == worker_id)
            {
                shard.worker = None;
            }
            if shard.pending_worker == Some(worker_id) {
                shard.pending_worker = None;
            }
        }
        cleanup_state.notify.notify_waiters();
    });

    if has_cached_index {
        let refresh_state = state.clone();
        let refresh_handle = handle.clone();
        tokio::spawn(async move {
            let root = {
                let guard = refresh_state.shards.lock().await;
                guard.get(&shard_id).map(|s| s.root.clone())
            };

            let Some(root) = root else {
                return;
            };

            let files = match collect_java_files(&root).await {
                Ok(files) => files,
                Err(err) => {
                    warn!(
                        shard_id,
                        worker_id = refresh_handle.worker_id,
                        root = %root.display(),
                        error = ?err,
                        "failed to load shard files for worker restart"
                    );
                    return;
                }
            };

            let revision = refresh_state.global_revision.load(Ordering::SeqCst);
            let resp = worker_call(&refresh_handle, Request::LoadFiles { revision, files }).await;
            if let Err(err) = resp {
                warn!(
                    shard_id,
                    worker_id = refresh_handle.worker_id,
                    error = ?err,
                    "failed to rehydrate worker file map"
                );
            }
        });
    }

    state.notify.notify_waiters();
    Ok(())
}

fn add_worker_restart_jitter(delay: Duration) -> Duration {
    let max_extra = delay / WORKER_RESTART_JITTER_DIVISOR;
    if max_extra.is_zero() {
        return delay;
    }

    let max_extra_ms: u64 = max_extra.as_millis().try_into().unwrap_or(u64::MAX);
    if max_extra_ms == 0 {
        return delay;
    }

    let mut bytes = [0u8; 8];
    if getrandom::getrandom(&mut bytes).is_err() {
        return delay;
    }

    let rand = u64::from_le_bytes(bytes);
    let extra_ms = rand % (max_extra_ms + 1);
    delay + Duration::from_millis(extra_ms)
}

async fn kill_and_reap_worker(
    shard_id: ShardId,
    attempt: u64,
    mut child: tokio::process::Child,
    reason: &'static str,
) -> Option<std::process::ExitStatus> {
    if let Err(err) = child.start_kill() {
        warn!(
            shard_id,
            attempt,
            reason = %reason,
            error = ?err,
            "failed to kill worker"
        );
    }

    match timeout(WORKER_KILL_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) => Some(status),
        Ok(Err(err)) => {
            warn!(
                shard_id,
                attempt,
                reason = %reason,
                error = ?err,
                "failed to wait for worker after kill"
            );
            None
        }
        Err(_) => {
            warn!(
                shard_id,
                attempt,
                reason = %reason,
                timeout = ?WORKER_KILL_TIMEOUT,
                "timed out waiting for worker after kill; detaching reap task"
            );
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
            None
        }
    }
}

async fn worker_supervisor_loop(
    state: Arc<RouterState>,
    shard_id: ShardId,
    shutdown_rx: &mut watch::Receiver<bool>,
) {
    let connect_arg = state.config.listen_addr.as_worker_connect_arg();
    let mut backoff =
        RestartBackoff::new(WORKER_RESTART_BACKOFF_INITIAL, WORKER_RESTART_BACKOFF_MAX);
    let mut attempt: u64 = 0;

    loop {
        if *shutdown_rx.borrow() {
            return;
        }

        let previous_worker_id = {
            let guard = state.shards.lock().await;
            guard
                .get(&shard_id)
                .and_then(|shard| shard.worker.as_ref().map(|w| w.worker_id))
        };

        attempt += 1;

        let mut cmd = Command::new(&state.config.worker_command);
        cmd.kill_on_drop(true);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.arg("--connect")
            .arg(&connect_arg)
            .arg("--shard-id")
            .arg(shard_id.to_string())
            .arg("--cache-dir")
            .arg(&state.config.cache_dir);
        cmd.arg("--max-rpc-bytes")
            .arg(state.config.max_rpc_bytes.to_string());

        if let Some(token) = state.config.auth_token.as_ref() {
            // Avoid passing secrets via argv. Instead, set the token in the child environment and
            // instruct the worker to read it.
            cmd.env("NOVA_WORKER_AUTH_TOKEN", token);
            cmd.arg("--auth-token-env").arg("NOVA_WORKER_AUTH_TOKEN");
        }

        if state.config.allow_insecure_tcp
            && matches!(
                state.config.listen_addr,
                ListenAddr::Tcp(TcpListenAddr::Plain(_))
            )
        {
            cmd.arg("--allow-insecure");
        }

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(err) => {
                let backoff_delay = backoff.next_delay();
                let delay = add_worker_restart_jitter(backoff_delay);
                warn!(
                    shard_id,
                    attempt,
                    backoff_delay = ?backoff_delay,
                    delay = ?delay,
                    worker_command = %state.config.worker_command.display(),
                    error = ?err,
                    "failed to spawn worker; retrying"
                );
                tokio::select! {
                    _ = shutdown_rx.changed() => {},
                    _ = tokio::time::sleep(delay) => {},
                }
                continue;
            }
        };
        info!(shard_id, pid = ?child.id(), "spawned worker process");

        if let Some(stdout) = child.stdout.take() {
            tokio::spawn(drain_worker_output(shard_id, "stdout", stdout));
        }
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(drain_worker_output(shard_id, "stderr", stderr));
        }

        enum SpawnEvent {
            Shutdown,
            Exited(std::process::ExitStatus),
            Connected {
                worker_id: WorkerId,
                connected_at: Instant,
            },
            HandshakeTimeout,
        }

        let handshake_deadline = Instant::now() + WORKER_HANDSHAKE_TIMEOUT;
        let spawn_event = loop {
            if *shutdown_rx.borrow() {
                break SpawnEvent::Shutdown;
            }

            if let Some(worker_id) = {
                let guard = state.shards.lock().await;
                guard
                    .get(&shard_id)
                    .and_then(|shard| shard.worker.as_ref().map(|w| w.worker_id))
            } {
                if Some(worker_id) != previous_worker_id {
                    break SpawnEvent::Connected {
                        worker_id,
                        connected_at: Instant::now(),
                    };
                }
            }

            tokio::select! {
                _ = shutdown_rx.changed() => {}
                status = child.wait() => {
                    match status {
                        Ok(status) => break SpawnEvent::Exited(status),
                        Err(err) => {
                            warn!(shard_id, attempt, error = ?err, "failed to wait on worker during handshake");
                            break SpawnEvent::HandshakeTimeout;
                        }
                    }
                }
                _ = state.notify.notified() => {}
                _ = tokio::time::sleep_until(handshake_deadline) => break SpawnEvent::HandshakeTimeout,
            }
        };

        let (stable_session, exit_status) = match spawn_event {
            SpawnEvent::Shutdown => {
                let _ = kill_and_reap_worker(shard_id, attempt, child, "shutdown").await;
                return;
            }
            SpawnEvent::HandshakeTimeout => {
                warn!(
                    shard_id,
                    attempt,
                    timeout = ?WORKER_HANDSHAKE_TIMEOUT,
                    "worker did not complete handshake; restarting"
                );
                let status =
                    kill_and_reap_worker(shard_id, attempt, child, "handshake-timeout").await;
                (false, status)
            }
            SpawnEvent::Exited(status) => {
                warn!(
                    shard_id,
                    attempt,
                    status = ?status,
                    "worker exited before handshake"
                );
                (false, Some(status))
            }
            SpawnEvent::Connected {
                worker_id,
                connected_at,
            } => {
                info!(shard_id, worker_id, attempt, "worker connected");

                enum SessionEvent {
                    Shutdown,
                    Exited(std::process::ExitStatus),
                    Disconnected,
                }

                let session_event = loop {
                    if *shutdown_rx.borrow() {
                        break SessionEvent::Shutdown;
                    }

                    let current_worker_id = {
                        let guard = state.shards.lock().await;
                        guard
                            .get(&shard_id)
                            .and_then(|shard| shard.worker.as_ref().map(|w| w.worker_id))
                    };
                    if current_worker_id != Some(worker_id) {
                        break SessionEvent::Disconnected;
                    }

                    tokio::select! {
                        _ = shutdown_rx.changed() => {}
                        status = child.wait() => {
                            match status {
                                Ok(status) => break SessionEvent::Exited(status),
                                Err(err) => {
                                    warn!(shard_id, worker_id, error = ?err, "failed to wait on worker");
                                    break SessionEvent::Disconnected;
                                }
                            }
                        }
                        _ = state.notify.notified() => {}
                    }
                };

                let session_duration = connected_at.elapsed();
                let stable = session_duration >= WORKER_SESSION_RESET_BACKOFF_AFTER;

                match session_event {
                    SessionEvent::Shutdown => {
                        let _ = kill_and_reap_worker(shard_id, attempt, child, "shutdown").await;
                        return;
                    }
                    SessionEvent::Disconnected => {
                        warn!(
                            shard_id,
                            worker_id,
                            session_duration = ?session_duration,
                            "worker disconnected; restarting"
                        );
                        let status =
                            kill_and_reap_worker(shard_id, attempt, child, "disconnected").await;
                        (stable, status)
                    }
                    SessionEvent::Exited(status) => {
                        warn!(
                            shard_id,
                            worker_id,
                            session_duration = ?session_duration,
                            status = ?status,
                            "worker exited; restarting"
                        );
                        let should_clear_worker = {
                            let mut guard = state.shards.lock().await;
                            guard.get_mut(&shard_id).is_some_and(|shard| {
                                let is_current = shard
                                    .worker
                                    .as_ref()
                                    .is_some_and(|w| w.worker_id == worker_id);
                                if is_current {
                                    shard.worker = None;
                                }
                                is_current
                            })
                        };
                        if should_clear_worker {
                            state.notify.notify_waiters();
                        }
                        (stable, Some(status))
                    }
                }
            }
        };

        if stable_session {
            backoff.reset();
        }

        if let Some(status) = exit_status {
            info!(shard_id, status = ?status, "scheduling worker restart after exit");
        }

        let backoff_delay = backoff.next_delay();
        let delay = add_worker_restart_jitter(backoff_delay);
        info!(shard_id, backoff_delay = ?backoff_delay, delay = ?delay, "restarting worker");
        tokio::select! {
            _ = shutdown_rx.changed() => {},
            _ = tokio::time::sleep(delay) => {},
        }
    }
}

async fn drain_worker_output<R>(shard_id: ShardId, label: &'static str, reader: R)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    // Worker output can be adversarially large (e.g. a single "line" with no newline). Bound
    // buffering so a misbehaving worker can't OOM the router via `read_until('\n')`.
    const MAX_WORKER_OUTPUT_LINE_BYTES: usize = 64 * 1024;
    const WORKER_OUTPUT_TRUNCATION_MARKER: &str = "<output truncated>";

    fn bounded_worker_output(bytes: &[u8]) -> std::borrow::Cow<'_, str> {
        let output = String::from_utf8_lossy(bytes);
        if output.len() <= MAX_WORKER_OUTPUT_LINE_BYTES {
            return output;
        }

        let mut owned = output.into_owned();
        let mut cut = MAX_WORKER_OUTPUT_LINE_BYTES;
        while cut > 0 && !owned.is_char_boundary(cut) {
            cut -= 1;
        }
        owned.truncate(cut);
        std::borrow::Cow::Owned(owned)
    }

    let mut reader = BufReader::new(reader);
    let mut buf = Vec::new();
    let mut discarding_until_newline = false;

    loop {
        let available = match reader.fill_buf().await {
            Ok(buf) => buf,
            Err(err) => {
                warn!(
                    target = "nova.worker.output",
                    shard_id,
                    stream = label,
                    error = ?err,
                    "worker output error"
                );
                return;
            }
        };

        if available.is_empty() {
            if !buf.is_empty() || discarding_until_newline {
                let line = bounded_worker_output(&buf);
                let line = line.trim_end_matches(&['\r', '\n'][..]);
                let line = if discarding_until_newline {
                    let mut out = line.to_string();
                    out.push_str(WORKER_OUTPUT_TRUNCATION_MARKER);
                    out
                } else {
                    line.to_string()
                };
                info!(
                    target = "nova.worker.output",
                    shard_id,
                    stream = label,
                    line = %line,
                    "worker output"
                );
            }
            return;
        }

        let mut consumed = 0;
        while consumed < available.len() {
            if discarding_until_newline {
                if let Some(pos) = available[consumed..].iter().position(|&b| b == b'\n') {
                    consumed += pos + 1;

                    let line = bounded_worker_output(&buf);
                    let line = line.trim_end_matches(&['\r', '\n'][..]);
                    let mut line = line.to_string();
                    line.push_str(WORKER_OUTPUT_TRUNCATION_MARKER);
                    info!(
                        target = "nova.worker.output",
                        shard_id,
                        stream = label,
                        line = %line,
                        "worker output"
                    );

                    buf.clear();
                    discarding_until_newline = false;
                } else {
                    consumed = available.len();
                }
                continue;
            }

            let newline_pos = available[consumed..].iter().position(|&b| b == b'\n');
            let take = newline_pos
                .map(|pos| pos + 1)
                .unwrap_or(available.len() - consumed);

            let remaining = MAX_WORKER_OUTPUT_LINE_BYTES.saturating_sub(buf.len());
            if take <= remaining {
                buf.extend_from_slice(&available[consumed..consumed + take]);
                consumed += take;

                if newline_pos.is_some() {
                    let line = bounded_worker_output(&buf);
                    let line = line.trim_end_matches(&['\r', '\n'][..]);
                    info!(
                        target = "nova.worker.output",
                        shard_id,
                        stream = label,
                        line = %line,
                        "worker output"
                    );
                    buf.clear();
                }
                continue;
            }

            // This line is longer than the maximum.
            if remaining > 0 {
                buf.extend_from_slice(&available[consumed..consumed + remaining]);
            }
            consumed += take;

            if newline_pos.is_some() {
                let line = bounded_worker_output(&buf);
                let line = line.trim_end_matches(&['\r', '\n'][..]);
                let mut line = line.to_string();
                line.push_str(WORKER_OUTPUT_TRUNCATION_MARKER);
                info!(
                    target = "nova.worker.output",
                    shard_id,
                    stream = label,
                    line = %line,
                    "worker output"
                );
                buf.clear();
            } else {
                discarding_until_newline = true;
            }
        }

        reader.consume(consumed);
    }
}

fn index_for_files(
    shard_id: ShardId,
    mut files: Vec<String>,
    override_file: Option<(String, String)>,
    cancel: &CancellationToken,
) -> std::result::Result<anyhow::Result<Vec<Symbol>>, Cancelled> {
    use nova_db::{FileId, NovaHir, SalsaDatabase, SourceRootId};

    Cancelled::check(cancel)?;

    files.sort();

    let db = SalsaDatabase::new();
    let root = SourceRootId::from_raw(shard_id);
    // Use a single `FileId` and overwrite inputs as we iterate to keep peak Salsa memo usage
    // bounded. This avoids accumulating a full per-file parse + HIR cache when indexing many
    // files for workspace symbol search.
    let file_id = FileId::from_raw(0);
    db.set_source_root(file_id, root);

    let override_file = override_file.map(|(path, text)| (path, Arc::new(text)));

    let mut symbols = Vec::new();
    for file in files {
        Cancelled::check(cancel)?;
        let text = if let Some((_, override_text)) =
            override_file.as_ref().filter(|(path, _)| path == &file)
        {
            Arc::clone(override_text)
        } else {
            match std::fs::read_to_string(&file)
                .with_context(|| format!("read {file:?}"))
                .map(Arc::new)
            {
                Ok(text) => text,
                Err(err) => return Ok(Err(err)),
            }
        };
        db.set_file_exists(file_id, true);
        db.set_file_content(file_id, text);

        let names = db.with_snapshot(|snap| snap.hir_symbol_names(file_id));
        for name in names.iter() {
            symbols.push(Symbol {
                name: name.clone(),
                path: file.clone(),
            });
        }
    }

    symbols.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.path.cmp(&b.path)));
    symbols.dedup();
    Ok(Ok(symbols))
}

fn build_global_symbols<'a>(
    shard_indexes: impl IntoIterator<Item = &'a ShardIndex>,
) -> Vec<Symbol> {
    let mut symbols = Vec::new();
    for shard in shard_indexes {
        symbols.extend(shard.symbols.iter().cloned());
    }
    symbols.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.path.cmp(&b.path)));
    symbols.dedup();
    symbols
}

thread_local! {
    static TRIGRAM_SCRATCH: RefCell<TrigramCandidateScratch> =
        RefCell::new(TrigramCandidateScratch::default());
}

#[derive(Debug, Clone)]
struct GlobalSymbolIndex {
    update_id: u64,
    symbols: Vec<Symbol>,
    trigram: TrigramIndex,
    prefix1: Vec<Vec<u32>>,
}

impl Default for GlobalSymbolIndex {
    fn default() -> Self {
        Self {
            update_id: 0,
            symbols: Vec::new(),
            trigram: TrigramIndexBuilder::new().build(),
            prefix1: vec![Vec::new(); 256],
        }
    }
}

impl GlobalSymbolIndex {
    fn new(symbols: Vec<Symbol>, update_id: u64) -> Self {
        let mut prefix1: Vec<Vec<u32>> = vec![Vec::new(); 256];
        let mut builder = TrigramIndexBuilder::new();

        for (id, sym) in symbols.iter().enumerate() {
            let id_u32: u32 = id
                .try_into()
                .unwrap_or_else(|_| panic!("symbol index too large: {id}"));

            builder.insert(id_u32, &sym.name);

            if let Some(&b0) = sym.name.as_bytes().first() {
                prefix1[b0.to_ascii_lowercase() as usize].push(id_u32);
            }
        }

        Self {
            update_id,
            symbols,
            trigram: builder.build(),
            prefix1,
        }
    }

    fn search(&self, query: &str, limit: usize) -> Vec<Symbol> {
        if limit == 0 || self.symbols.is_empty() {
            return Vec::new();
        }

        if query.is_empty() {
            return self.symbols.iter().take(limit).cloned().collect();
        }

        let query_bytes = query.as_bytes();
        let query_first = query_bytes.first().copied().map(|b| b.to_ascii_lowercase());
        let mut matcher = FuzzyMatcher::new(query);

        let mut scored = BinaryHeap::with_capacity(limit);

        if query_bytes.len() < 3 {
            if let Some(b0) = query_first {
                let bucket = &self.prefix1[b0 as usize];
                if !bucket.is_empty() {
                    self.score_candidates_top_k(
                        bucket.iter().copied(),
                        &mut matcher,
                        limit,
                        &mut scored,
                    );
                    return self.finish_top_k(scored, limit);
                }
            }

            let scan_limit = FALLBACK_SCAN_LIMIT.min(self.symbols.len());
            self.score_candidates_top_k(
                (0..scan_limit).map(|id| id as u32),
                &mut matcher,
                limit,
                &mut scored,
            );
            return self.finish_top_k(scored, limit);
        }

        let have_trigram_candidates = TRIGRAM_SCRATCH.with(|scratch| {
            let mut scratch = scratch.borrow_mut();
            let candidates = self.trigram.candidates_with_scratch(query, &mut *scratch);
            if candidates.is_empty() {
                false
            } else {
                self.score_candidates_top_k(
                    candidates.iter().copied(),
                    &mut matcher,
                    limit,
                    &mut scored,
                );
                true
            }
        });

        if have_trigram_candidates {
            return self.finish_top_k(scored, limit);
        }

        if let Some(b0) = query_first {
            let bucket = &self.prefix1[b0 as usize];
            if !bucket.is_empty() {
                self.score_candidates_top_k(
                    bucket.iter().copied(),
                    &mut matcher,
                    limit,
                    &mut scored,
                );
                return self.finish_top_k(scored, limit);
            }
        }

        let scan_limit = FALLBACK_SCAN_LIMIT.min(self.symbols.len());
        self.score_candidates_top_k(
            (0..scan_limit).map(|id| id as u32),
            &mut matcher,
            limit,
            &mut scored,
        );
        self.finish_top_k(scored, limit)
    }

    fn score_candidates_top_k<'a>(
        &'a self,
        ids: impl IntoIterator<Item = u32>,
        matcher: &mut FuzzyMatcher,
        limit: usize,
        out: &mut BinaryHeap<ScoredSymbolHeapItem<'a>>,
    ) {
        if limit == 0 {
            return;
        }

        for id in ids {
            let Some(sym) = self.symbols.get(id as usize) else {
                continue;
            };
            if let Some(score) = matcher.score(&sym.name) {
                let scored = LocalScoredSymbol { id, score };
                if out.len() < limit {
                    out.push(ScoredSymbolHeapItem {
                        index: self,
                        scored,
                    });
                    continue;
                }

                // The heap is maintained such that `peek()` returns the *worst* element in the
                // current top-k set (i.e. the max element under `cmp_scored`).
                let Some(worst) = out.peek() else {
                    continue;
                };

                if self.cmp_scored(&scored, &worst.scored) == std::cmp::Ordering::Less {
                    out.pop();
                    out.push(ScoredSymbolHeapItem {
                        index: self,
                        scored,
                    });
                }
            }
        }
    }

    fn finish_top_k<'a>(
        &'a self,
        scored: BinaryHeap<ScoredSymbolHeapItem<'a>>,
        limit: usize,
    ) -> Vec<Symbol> {
        let scored = scored.into_iter().map(|item| item.scored).collect();
        self.finish(scored, limit)
    }

    fn finish(&self, mut scored: Vec<LocalScoredSymbol>, limit: usize) -> Vec<Symbol> {
        if scored.len() > limit {
            // Avoid sorting the entire result set on every keystroke.
            scored.select_nth_unstable_by(limit - 1, |a, b| self.cmp_scored(a, b));
            scored.truncate(limit);
        }

        scored.sort_by(|a, b| self.cmp_scored(a, b));

        scored
            .into_iter()
            .take(limit)
            .filter_map(|s| self.symbols.get(s.id as usize).cloned())
            .collect()
    }

    fn cmp_scored(&self, a: &LocalScoredSymbol, b: &LocalScoredSymbol) -> std::cmp::Ordering {
        b.score.rank_key().cmp(&a.score.rank_key()).then_with(|| {
            let a_sym = &self.symbols[a.id as usize];
            let b_sym = &self.symbols[b.id as usize];
            a_sym
                .name
                .len()
                .cmp(&b_sym.name.len())
                .then_with(|| a_sym.name.cmp(&b_sym.name))
                .then_with(|| a_sym.path.cmp(&b_sym.path))
                .then_with(|| a.id.cmp(&b.id))
        })
    }

    #[cfg(test)]
    fn score_candidates_all(
        &self,
        ids: impl IntoIterator<Item = u32>,
        matcher: &mut FuzzyMatcher,
        out: &mut Vec<LocalScoredSymbol>,
    ) {
        for id in ids {
            let Some(sym) = self.symbols.get(id as usize) else {
                continue;
            };
            if let Some(score) = matcher.score(&sym.name) {
                out.push(LocalScoredSymbol { id, score });
            }
        }
    }

    /// Baseline implementation used by tests to verify the streaming top-k selector returns
    /// identical results to the previous "collect-all then select_nth" approach.
    #[cfg(test)]
    fn search_full(&self, query: &str, limit: usize) -> Vec<Symbol> {
        if limit == 0 || self.symbols.is_empty() {
            return Vec::new();
        }

        if query.is_empty() {
            return self.symbols.iter().take(limit).cloned().collect();
        }

        let query_bytes = query.as_bytes();
        let query_first = query_bytes.first().copied().map(|b| b.to_ascii_lowercase());
        let mut matcher = FuzzyMatcher::new(query);

        let mut scored = Vec::new();

        if query_bytes.len() < 3 {
            if let Some(b0) = query_first {
                let bucket = &self.prefix1[b0 as usize];
                if !bucket.is_empty() {
                    self.score_candidates_all(bucket.iter().copied(), &mut matcher, &mut scored);
                    return self.finish(scored, limit);
                }
            }

            let scan_limit = FALLBACK_SCAN_LIMIT.min(self.symbols.len());
            self.score_candidates_all(
                (0..scan_limit).map(|id| id as u32),
                &mut matcher,
                &mut scored,
            );
            return self.finish(scored, limit);
        }

        let mut candidate_scratch = TrigramCandidateScratch::default();
        let candidates = self
            .trigram
            .candidates_with_scratch(query, &mut candidate_scratch);

        if !candidates.is_empty() {
            self.score_candidates_all(candidates.iter().copied(), &mut matcher, &mut scored);
            return self.finish(scored, limit);
        }

        if let Some(b0) = query_first {
            let bucket = &self.prefix1[b0 as usize];
            if !bucket.is_empty() {
                self.score_candidates_all(bucket.iter().copied(), &mut matcher, &mut scored);
                return self.finish(scored, limit);
            }
        }

        let scan_limit = FALLBACK_SCAN_LIMIT.min(self.symbols.len());
        self.score_candidates_all(
            (0..scan_limit).map(|id| id as u32),
            &mut matcher,
            &mut scored,
        );
        self.finish(scored, limit)
    }
}

#[derive(Debug, Clone)]
struct LocalScoredSymbol {
    id: u32,
    score: MatchScore,
}

#[derive(Debug, Clone)]
struct ScoredSymbolHeapItem<'a> {
    index: &'a GlobalSymbolIndex,
    scored: LocalScoredSymbol,
}

impl PartialEq for ScoredSymbolHeapItem<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}

impl Eq for ScoredSymbolHeapItem<'_> {}

impl PartialOrd for ScoredSymbolHeapItem<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ScoredSymbolHeapItem<'_> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        debug_assert!(std::ptr::eq(self.index, other.index));
        self.index.cmp_scored(&self.scored, &other.scored)
    }
}

async fn write_global_symbols(
    dst: &RwLock<GlobalSymbolIndex>,
    symbols: Vec<Symbol>,
    update_id: u64,
) {
    let mut guard = dst.write().await;
    if update_id < guard.update_id {
        return;
    }
    *guard = GlobalSymbolIndex::new(symbols, update_id);
}

async fn collect_java_file_paths(root: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let mut read_dir = tokio::fs::read_dir(&dir)
            .await
            .with_context(|| format!("read_dir {dir:?}"))?;
        while let Some(entry) = read_dir
            .next_entry()
            .await
            .with_context(|| format!("next_entry {dir:?}"))?
        {
            let path = entry.path();
            let meta = entry
                .metadata()
                .await
                .with_context(|| format!("metadata {path:?}"))?;
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file() && path.extension().and_then(|s| s.to_str()) == Some("java") {
                out.push(path.to_string_lossy().to_string());
            }
        }
    }

    Ok(out)
}

async fn collect_java_files(root: &Path) -> Result<Vec<FileText>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let mut read_dir = tokio::fs::read_dir(&dir)
            .await
            .with_context(|| format!("read_dir {dir:?}"))?;
        while let Some(entry) = read_dir
            .next_entry()
            .await
            .with_context(|| format!("next_entry {dir:?}"))?
        {
            let path = entry.path();
            let meta = entry
                .metadata()
                .await
                .with_context(|| format!("metadata {path:?}"))?;
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file() && path.extension().and_then(|s| s.to_str()) == Some("java") {
                let text = tokio::fs::read_to_string(&path)
                    .await
                    .with_context(|| format!("read {path:?}"))?;
                out.push(FileText {
                    path: path.to_string_lossy().to_string(),
                    text,
                });
            }
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use tokio::io::AsyncWriteExt;
    use tracing::field::{Field, Visit};
    use tracing_subscriber::layer::Context;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::Layer;

    #[test]
    fn distributed_router_config_debug_does_not_expose_auth_token() {
        let token = "super-secret-token";
        let config = DistributedRouterConfig {
            listen_addr: ListenAddr::Tcp(TcpListenAddr::Plain("127.0.0.1:0".parse().unwrap())),
            worker_command: PathBuf::from("nova-worker"),
            cache_dir: std::env::temp_dir(),
            auth_token: Some(token.to_string()),
            allow_insecure_tcp: false,
            max_rpc_bytes: DEFAULT_MAX_RPC_BYTES,
            max_inflight_handshakes: DEFAULT_MAX_INFLIGHT_HANDSHAKES,
            max_worker_connections: DEFAULT_MAX_WORKER_CONNECTIONS,
            #[cfg(feature = "tls")]
            tls_client_cert_fingerprint_allowlist: Default::default(),
            spawn_workers: false,
        };

        let output = format!("{config:?}");
        assert!(
            !output.contains(token),
            "DistributedRouterConfig debug output leaked auth token: {output}"
        );
        assert!(
            output.contains("auth_present"),
            "DistributedRouterConfig debug output should include auth presence indicator: {output}"
        );
    }

    #[test]
    fn global_symbol_search_prefers_prefix_matches() {
        let symbols = vec![
            Symbol {
                name: "foobar".into(),
                path: "a.java".into(),
            },
            Symbol {
                name: "barfoo".into(),
                path: "b.java".into(),
            },
        ];

        let index = GlobalSymbolIndex::new(symbols, 0);
        let results = index.search("foo", 10);
        assert_eq!(results[0].name, "foobar");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn drain_worker_output_truncates_overlong_lines() {
        const MAX_WORKER_OUTPUT_LINE_BYTES: usize = 64 * 1024;
        const WORKER_OUTPUT_TRUNCATION_MARKER: &str = "<output truncated>";

        #[derive(Clone)]
        struct CaptureLayer {
            lines: Arc<Mutex<Vec<String>>>,
        }

        impl<S> Layer<S> for CaptureLayer
        where
            S: tracing::Subscriber,
        {
            fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
                Some(tracing::level_filters::LevelFilter::INFO)
            }

            fn register_callsite(
                &self,
                _metadata: &'static tracing::Metadata<'static>,
            ) -> tracing::subscriber::Interest {
                tracing::subscriber::Interest::always()
            }

            fn enabled(&self, _metadata: &tracing::Metadata<'_>, _ctx: Context<'_, S>) -> bool {
                true
            }

            fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
                struct Visitor {
                    target: Option<String>,
                    line: Option<String>,
                }

                impl Visit for Visitor {
                    fn record_str(&mut self, field: &Field, value: &str) {
                        match field.name() {
                            "target" => self.target = Some(value.to_string()),
                            "line" => self.line = Some(value.to_string()),
                            _ => {}
                        }
                    }

                    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                        match field.name() {
                            "target" => self.target = Some(format!("{value:?}")),
                            "line" => self.line = Some(format!("{value:?}")),
                            _ => {}
                        }
                    }
                }

                let mut visitor = Visitor {
                    target: None,
                    line: None,
                };
                event.record(&mut visitor);
                let meta_target = event.metadata().target();
                let matches_target = meta_target == "nova.worker.output"
                    || visitor.target.as_deref() == Some("nova.worker.output");
                if matches_target {
                    if let Some(line) = visitor.line {
                        self.lines.lock().unwrap().push(line);
                    }
                }
            }
        }

        let lines = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(CaptureLayer { lines: lines.clone() });
        let _guard = tracing::subscriber::set_default(subscriber);
        tracing::callsite::rebuild_interest_cache();

        tracing::info!(target = "nova.worker.output", line = %"probe", "worker output");
        assert_eq!(lines.lock().unwrap().len(), 1, "capture layer did not receive probe event");
        lines.lock().unwrap().clear();

        let (mut writer, reader) = tokio::io::duplex(64 * 1024);
        let task = tokio::spawn(drain_worker_output(1, "stdout", reader));

        let oversized = vec![b'a'; 200 * 1024];
        writer.write_all(&oversized).await.unwrap();
        writer.write_all(b"\n").await.unwrap();
        writer.shutdown().await.unwrap();

        task.await.unwrap();

        let captured = lines.lock().unwrap();
        assert_eq!(captured.len(), 1, "expected exactly one logged line");

        let line = &captured[0];
        assert!(
            line.contains(WORKER_OUTPUT_TRUNCATION_MARKER),
            "expected truncation marker in output, got: {line:?}"
        );
        assert!(
            line.len() <= MAX_WORKER_OUTPUT_LINE_BYTES + WORKER_OUTPUT_TRUNCATION_MARKER.len() + 2,
            "expected logged line to be bounded (got {}, limit {})",
            line.len(),
            MAX_WORKER_OUTPUT_LINE_BYTES + WORKER_OUTPUT_TRUNCATION_MARKER.len() + 2
        );
    }

    #[test]
    fn global_symbol_search_supports_acronym_queries() {
        let symbols = vec![
            Symbol {
                name: "FooBar".into(),
                path: "a.java".into(),
            },
            Symbol {
                name: "foobar".into(),
                path: "b.java".into(),
            },
        ];

        let index = GlobalSymbolIndex::new(symbols, 0);
        let results = index.search("fb", 10);
        assert_eq!(results[0].name, "FooBar");
    }

    #[test]
    fn global_symbol_search_filters_by_trigrams_for_long_queries() {
        let symbols = vec![
            Symbol {
                name: "HashMap".into(),
                path: "a.java".into(),
            },
            Symbol {
                name: "FooBar".into(),
                path: "b.java".into(),
            },
        ];

        let index = GlobalSymbolIndex::new(symbols, 0);
        let results = index.search("Hash", 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "HashMap");
    }

    #[test]
    fn global_symbol_search_top_k_orders_ties_correctly() {
        let mut symbols = Vec::new();

        // Craft a large candidate set where every symbol is a prefix match for "foo" and has the
        // same length, so ranking falls through to the name/path/id tie-breakers.
        //
        // Include duplicates for the lexicographically-first name to ensure path/id tie-breaking
        // is exercised.
        symbols.push(Symbol {
            name: "fooaa".into(),
            path: "b.java".into(),
        }); // id 0
        symbols.push(Symbol {
            name: "fooaa".into(),
            path: "a.java".into(),
        }); // id 1
        symbols.push(Symbol {
            name: "fooaa".into(),
            path: "a.java".into(),
        }); // id 2 (duplicate name+path, should be ordered by id)

        for a in b'a'..=b'z' {
            for b in b'a'..=b'z' {
                // Skip "fooaa" because we inserted a few duplicates above.
                if a == b'a' && b == b'a' {
                    continue;
                }
                let name = format!("foo{}{}", a as char, b as char);
                symbols.push(Symbol {
                    name,
                    path: format!("{a}{b}.java"),
                });
            }
        }

        let index = GlobalSymbolIndex::new(symbols, 0);
        let results = index.search("foo", 5);

        let formatted: Vec<(String, String)> = results
            .into_iter()
            .map(|s| (s.name.to_string(), s.path.to_string()))
            .collect();

        assert_eq!(
            formatted,
            vec![
                ("fooaa".into(), "a.java".into()),
                ("fooaa".into(), "a.java".into()),
                ("fooaa".into(), "b.java".into()),
                ("fooab".into(), "9798.java".into()),
                ("fooac".into(), "9799.java".into()),
            ]
        );
    }

    #[test]
    fn global_symbol_search_streaming_top_k_matches_full_collection() {
        let mut symbols = Vec::new();

        // Ensure there are many matches and lots of tie-breaking.
        symbols.push(Symbol {
            name: "fooaa".into(),
            path: "b.java".into(),
        });
        symbols.push(Symbol {
            name: "fooaa".into(),
            path: "a.java".into(),
        });
        symbols.push(Symbol {
            name: "fooaa".into(),
            path: "a.java".into(),
        });

        for a in b'a'..=b'z' {
            for b in b'a'..=b'z' {
                if a == b'a' && b == b'a' {
                    continue;
                }
                let name = format!("foo{}{}", a as char, b as char);
                symbols.push(Symbol {
                    name,
                    path: format!("{a}{b}.java"),
                });
            }
        }

        let index = GlobalSymbolIndex::new(symbols, 0);

        // Exercise both the prefix-bucket (len < 3) and trigram-filtered paths.
        for query in ["f", "fo", "foo"] {
            for limit in [1_usize, 2, 5, 25, 200] {
                assert_eq!(
                    index.search(query, limit),
                    index.search_full(query, limit),
                    "mismatch for query={query:?} limit={limit}"
                );
            }
        }
    }

    #[test]
    fn global_symbol_search_reuses_trigram_scratch_across_queries() {
        let symbols = vec![
            Symbol {
                name: "HashMap".into(),
                path: "a.java".into(),
            },
            Symbol {
                name: "FooBar".into(),
                path: "b.java".into(),
            },
            Symbol {
                name: "Vector".into(),
                path: "c.java".into(),
            },
        ];

        let index = GlobalSymbolIndex::new(symbols, 0);

        let cases = [
            ("Hash", Some("HashMap")),
            // Acronym-style query where trigram intersection is likely empty.
            ("fbr", Some("FooBar")),
            ("zzz", None),
            ("Vec", Some("Vector")),
        ];

        // Run the queries repeatedly to ensure scratch state doesn't leak between calls.
        for _ in 0..3 {
            for (query, expected) in cases {
                let results = index.search(query, 10);
                match expected {
                    Some(name) => assert_eq!(
                        results.first().map(|sym| sym.name.as_str()),
                        Some(name),
                        "unexpected first result for query={query:?}: {results:?}"
                    ),
                    None => assert!(
                        results.is_empty(),
                        "expected no results for query={query:?}, got {results:?}"
                    ),
                }
            }
        }
    }
}
