use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context};
use nova_bugreport::{install_panic_hook, PanicHookConfig};
use nova_config::{init_tracing_with_config, NovaConfig};
use nova_fuzzy::{FuzzyMatcher, MatchScore, TrigramIndex, TrigramIndexBuilder};
use nova_remote_proto::{
    transport, FileText, RpcMessage, ScoredSymbol, ShardId, ShardIndex, Symbol, WorkerId,
    WorkerStats,
};
use nova_scheduler::{CancellationToken, Cancelled, Scheduler, SchedulerConfig, TaskError};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot, watch, Mutex, Notify, RwLock, Semaphore};
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

const WORKER_RESTART_BACKOFF_INITIAL: Duration = Duration::from_millis(50);
const WORKER_RESTART_BACKOFF_MAX: Duration = Duration::from_secs(5);
const WORKER_SESSION_RESET_BACKOFF_AFTER: Duration = Duration::from_secs(10);
const WORKER_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const WORKER_KILL_TIMEOUT: Duration = Duration::from_secs(2);
const WORKER_RESTART_JITTER_DIVISOR: u32 = 4;

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
/// - answering workspace symbol queries by merging per-shard top-k results
pub struct QueryRouter {
    inner: RouterMode,
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

    pub async fn index_workspace(&self) -> Result<()> {
        match &self.inner {
            RouterMode::InProcess(router) => router.index_workspace().await,
            RouterMode::Distributed(router) => router.index_workspace().await,
        }
    }

    pub async fn update_file(&self, path: PathBuf, text: String) -> Result<()> {
        match &self.inner {
            RouterMode::InProcess(router) => router.update_file(path, text).await,
            RouterMode::Distributed(router) => router.update_file(path, text).await,
        }
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
        let token = self.next_index_token().await;
        let revision = self.global_revision.fetch_add(1, Ordering::SeqCst) + 1;
        let mut indexes = HashMap::new();
        for (shard_id, root) in self.layout.source_roots.iter().enumerate() {
            let files = collect_java_files(&root.path).await?;
            let shard_id = shard_id as ShardId;
            let task = self
                .scheduler
                .spawn_background_with_token(token.clone(), move |token| {
                    Cancelled::check(&token)?;
                    let symbols = index_for_files(shard_id, files);
                    Cancelled::check(&token)?;
                    Ok(symbols)
                });
            let symbols = match task.join().await {
                Ok(symbols) => symbols,
                Err(TaskError::Cancelled) => return Ok(()),
                Err(TaskError::Panicked) => return Err(anyhow!("indexing task panicked")),
                Err(TaskError::DeadlineExceeded(_)) => {
                    return Err(anyhow!("indexing task exceeded deadline"))
                }
            };
            indexes.insert(
                shard_id as ShardId,
                ShardIndex {
                    shard_id: shard_id as ShardId,
                    revision,
                    index_generation: revision,
                    symbols,
                },
            );
        }

        if token.is_cancelled() {
            return Ok(());
        }

        {
            let mut guard = self.shard_indexes.lock().await;
            *guard = indexes.clone();
        }

        let symbols = build_global_symbols(indexes.values());
        write_global_symbols(&self.global_symbols, symbols).await;
        Ok(())
    }

    async fn update_file(&self, path: PathBuf, text: String) -> Result<()> {
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
            collect_java_files(&self.layout.source_roots[shard_id as usize].path).await?;
        let path_str = path.to_string_lossy().to_string();
        if let Some(file) = shard_files.iter_mut().find(|f| f.path == path_str) {
            file.text = text;
        } else {
            shard_files.push(FileText {
                path: path_str,
                text,
            });
        }

        let task = self
            .scheduler
            .spawn_background_with_token(token.clone(), move |token| {
                Cancelled::check(&token)?;
                let symbols = index_for_files(shard_id, shard_files);
                Cancelled::check(&token)?;
                Ok(symbols)
            });
        let symbols = match task.join().await {
            Ok(symbols) => symbols,
            Err(TaskError::Cancelled) => return Ok(()),
            Err(TaskError::Panicked) => return Err(anyhow!("indexing task panicked")),
            Err(TaskError::DeadlineExceeded(_)) => {
                return Err(anyhow!("indexing task exceeded deadline"))
            }
        };

        if token.is_cancelled() {
            return Ok(());
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
        write_global_symbols(&self.global_symbols, symbols).await;
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
}

struct RouterState {
    config: DistributedRouterConfig,
    layout: WorkspaceLayout,
    next_worker_id: AtomicU32,
    global_revision: AtomicU64,
    shards: Mutex<HashMap<ShardId, ShardState>>,
    notify: Notify,
    handshake_semaphore: Arc<Semaphore>,
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
    tx: mpsc::UnboundedSender<WorkerRequest>,
}

struct WorkerRequest {
    message: RpcMessage,
    reply: Option<oneshot::Sender<Result<RpcMessage>>>,
}

impl WorkerHandle {
    async fn request(&self, message: RpcMessage) -> Result<RpcMessage> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(WorkerRequest {
                message,
                reply: Some(tx),
            })
            .map_err(|_| {
                anyhow!(
                    "worker {} (shard {}) disconnected",
                    self.worker_id,
                    self.shard_id
                )
            })?;
        rx.await.context("worker response channel closed")?
    }

    fn notify(&self, message: RpcMessage) -> Result<()> {
        self.tx
            .send(WorkerRequest {
                message,
                reply: None,
            })
            .map_err(|_| {
                anyhow!(
                    "worker {} (shard {}) disconnected",
                    self.worker_id,
                    self.shard_id
                )
            })?;
        Ok(())
    }
}

impl DistributedRouter {
    async fn new(config: DistributedRouterConfig, layout: WorkspaceLayout) -> Result<Self> {
        let mut config = config;
        if config.spawn_workers && config.auth_token.is_none() {
            config.auth_token = Some(ipc_security::generate_auth_token()?);
        }

        config.validate()?;
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

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
            notify: Notify::new(),
            handshake_semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_HANDSHAKES)),
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
        })
    }

    async fn index_workspace(&self) -> Result<()> {
        let revision = self.state.global_revision.fetch_add(1, Ordering::SeqCst) + 1;
        let shard_ids: Vec<ShardId> =
            (0..self.state.layout.source_roots.len() as ShardId).collect();
        let mut results = Vec::new();

        for shard_id in shard_ids {
            let root = {
                let guard = self.state.shards.lock().await;
                guard
                    .get(&shard_id)
                    .map(|s| s.root.clone())
                    .ok_or_else(|| anyhow!("unknown shard {shard_id}"))?
            };
            let files = collect_java_files(&root).await?;
            let worker = wait_for_worker(self.state.clone(), shard_id).await?;
            results.push((shard_id, worker, files));
        }

        for (shard_id, worker, files) in results {
            let resp = worker
                .request(RpcMessage::IndexShard { revision, files })
                .await?;
            match resp {
                RpcMessage::ShardIndexInfo(info) => {
                    if info.shard_id != shard_id {
                        self.disconnect_worker(&worker).await;
                        return Err(anyhow!(
                            "worker returned index info for wrong shard {} (expected {shard_id})",
                            info.shard_id
                        ));
                    }
                }
                other => return Err(anyhow!("unexpected worker response: {other:?}")),
            }
        }

        Ok(())
    }

    async fn update_file(&self, path: PathBuf, text: String) -> Result<()> {
        let shard_id = self
            .state
            .layout
            .source_roots
            .iter()
            .enumerate()
            .find_map(|(id, root)| path.starts_with(&root.path).then_some(id as ShardId))
            .ok_or_else(|| anyhow!("file {path:?} not in any source root"))?;

        let revision = self.state.global_revision.fetch_add(1, Ordering::SeqCst) + 1;
        let worker = wait_for_worker(self.state.clone(), shard_id).await?;
        let file = FileText {
            path: path.to_string_lossy().to_string(),
            text,
        };

        let resp = worker
            .request(RpcMessage::UpdateFile { revision, file })
            .await?;
        match resp {
            RpcMessage::ShardIndexInfo(info) => {
                if info.shard_id != shard_id {
                    self.disconnect_worker(&worker).await;
                    return Err(anyhow!(
                        "worker returned index info for wrong shard {} (expected {shard_id})",
                        info.shard_id
                    ));
                }
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
            let resp = worker.request(RpcMessage::GetWorkerStats).await?;
            match resp {
                RpcMessage::WorkerStats(ws) => {
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
        let workers: Vec<(ShardId, WorkerHandle)> = {
            let guard = self.state.shards.lock().await;
            guard
                .iter()
                .filter_map(|(shard_id, shard)| shard.worker.clone().map(|w| (*shard_id, w)))
                .collect()
        };

        if workers.is_empty() {
            return Vec::new();
        }

        let mut tasks = JoinSet::new();
        let query = query.to_string();
        for (shard_id, worker) in workers {
            let query = query.clone();
            let worker_id = worker.worker_id;
            tasks.spawn(async move {
                let resp = worker
                    .request(RpcMessage::SearchSymbols {
                        query,
                        limit: WORKSPACE_SYMBOL_LIMIT as u32,
                    })
                    .await;
                (shard_id, worker_id, resp)
            });
        }

        let mut merged = Vec::new();
        while let Some(res) = tasks.join_next().await {
            match res {
                Ok((_, _, Ok(RpcMessage::SearchSymbolsResult { items }))) => {
                    merged.extend(items);
                }
                Ok((shard_id, worker_id, Ok(RpcMessage::Error { message }))) => {
                    warn!(
                        shard_id,
                        worker_id,
                        message = %message,
                        "worker returned error for symbol search"
                    );
                }
                Ok((shard_id, worker_id, Ok(other))) => {
                    warn!(
                        shard_id,
                        worker_id,
                        response = ?other,
                        "unexpected worker response for symbol search"
                    );
                }
                Ok((shard_id, worker_id, Err(err))) => {
                    warn!(shard_id, worker_id, error = ?err, "symbol search request failed");
                }
                Err(err) => {
                    error!(error = ?err, "symbol search task failed");
                }
            }
        }

        if query.is_empty() {
            merged.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.path.cmp(&b.path)));
        } else {
            merged.sort_by(|a, b| scored_symbol_cmp(a, b));
        }
        merged.dedup_by(|a, b| a.name == b.name && a.path == b.path);
        merged
            .into_iter()
            .take(WORKSPACE_SYMBOL_LIMIT)
            .map(|s| Symbol {
                name: s.name,
                path: s.path,
            })
            .collect()
    }

    async fn shutdown(&self) -> Result<()> {
        let _ = self.shutdown_tx.send(true);

        {
            let guard = self.state.shards.lock().await;
            for worker in guard.values().filter_map(|s| s.worker.as_ref()) {
                let _ = worker.notify(RpcMessage::Shutdown);
            }
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
        let _ = worker.notify(RpcMessage::Shutdown);

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

fn scored_symbol_cmp(a: &ScoredSymbol, b: &ScoredSymbol) -> std::cmp::Ordering {
    b.rank_key
        .cmp(&a.rank_key)
        .then_with(|| a.name.len().cmp(&b.name.len()))
        .then_with(|| a.name.cmp(&b.name))
        .then_with(|| a.path.cmp(&b.path))
}

async fn wait_for_worker(state: Arc<RouterState>, shard_id: ShardId) -> Result<WorkerHandle> {
    let deadline = Duration::from_secs(10);
    timeout(deadline, async {
        loop {
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
            state.notify.notified().await;
        }
    })
    .await
    .context("timed out waiting for worker")?
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
                let Ok(permit) = state.handshake_semaphore.clone().try_acquire_owned() else {
                    warn!(
                        socket_path = %path.display(),
                        "dropping incoming unix connection: too many pending handshakes"
                    );
                    continue;
                };
                let conn_state = state.clone();
                let socket_path = path.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    if let Err(err) =
                        handle_new_connection(conn_state, boxed, WorkerIdentity::Unauthenticated)
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
                let Ok(permit) = state.handshake_semaphore.clone().try_acquire_owned() else {
                    warn!(
                        pipe_name = %name,
                        "dropping incoming named pipe connection: too many pending handshakes"
                    );
                    continue;
                };
                let conn_state = state.clone();
                let pipe_name = name.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    if let Err(err) =
                        handle_new_connection(conn_state, stream, WorkerIdentity::Unauthenticated)
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

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    return Ok(());
                }
            }
            res = listener.accept() => {
                let (stream, peer_addr) = res.with_context(|| format!("accept tcp {addr}"))?;
                let Ok(permit) = state.handshake_semaphore.clone().try_acquire_owned() else {
                    warn!(
                        peer_addr = %peer_addr,
                        "dropping incoming tcp connection: too many pending handshakes"
                    );
                    continue;
                };
                let conn_state = state.clone();
                let cfg = cfg.clone();
                tokio::spawn(async move {
                    let _permit = permit;
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
                    if let Err(err) = handle_new_connection(conn_state, boxed, identity).await {
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
    mut stream: BoxedStream,
    identity: WorkerIdentity,
) -> Result<()> {
    #[cfg(not(feature = "tls"))]
    let _ = &identity;

    let payload = timeout(
        WORKER_HANDSHAKE_TIMEOUT,
        transport::read_payload(&mut stream),
    )
    .await
    .context("timed out waiting for WorkerHello")??;
    let hello = match nova_remote_proto::decode_message(&payload) {
        Ok(message) => message,
        Err(v2_err) => {
            if let Ok(frame) = nova_remote_proto::v3::decode_wire_frame(&payload) {
                if matches!(frame, nova_remote_proto::v3::WireFrame::Hello(_)) {
                    let reject = nova_remote_proto::v3::WireFrame::Reject(
                        nova_remote_proto::v3::HandshakeReject {
                            code: nova_remote_proto::v3::RejectCode::UnsupportedVersion,
                            message: "router only supports legacy_v2 protocol".into(),
                        },
                    );
                    if let Ok(bytes) = nova_remote_proto::v3::encode_wire_frame(&reject) {
                        let _ = timeout(
                            WORKER_HANDSHAKE_TIMEOUT,
                            transport::write_payload(&mut stream, &bytes),
                        )
                        .await;
                    }
                    return Err(anyhow!(
                        "received v3 worker hello; this router only supports legacy_v2"
                    ));
                }
            }
            return Err(v2_err).context("decode legacy_v2 worker hello");
        }
    };
    let (shard_id, auth_token, has_cached_index) = match hello {
        RpcMessage::WorkerHello {
            shard_id,
            auth_token,
            has_cached_index,
        } => (shard_id, auth_token, has_cached_index),
        other => return Err(anyhow!("expected WorkerHello, got {other:?}")),
    };

    if let Some(expected) = state.config.auth_token.as_ref() {
        if auth_token.as_deref() != Some(expected.as_str()) {
            warn!(shard_id, "worker authentication failed");
            timeout(
                WORKER_HANDSHAKE_TIMEOUT,
                transport::write_message(
                    &mut stream,
                    &RpcMessage::Error {
                        message: "authentication failed".into(),
                    },
                ),
            )
            .await
            .ok();
            return Err(anyhow!("worker authentication failed for shard {shard_id}"));
        }
    }

    #[cfg(feature = "tls")]
    {
        let allowlist = &state.config.tls_client_cert_fingerprint_allowlist;
        let shard_allowlist = allowlist.shards.get(&shard_id);
        let enforce_allowlist = !allowlist.global.is_empty() || shard_allowlist.is_some();

        if enforce_allowlist {
            let Some(fingerprint) = identity.tls_client_cert_fingerprint() else {
                timeout(
                    WORKER_HANDSHAKE_TIMEOUT,
                    transport::write_message(
                        &mut stream,
                        &RpcMessage::Error {
                            message: "mTLS client certificate required".into(),
                        },
                    ),
                )
                .await
                .ok();
                return Err(anyhow!("shard {shard_id} requires mTLS client identity"));
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
                timeout(
                    WORKER_HANDSHAKE_TIMEOUT,
                    transport::write_message(
                        &mut stream,
                        &RpcMessage::Error {
                            message: "shard authorization failed".into(),
                        },
                    ),
                )
                .await
                .ok();
                return Err(anyhow!(
                    "worker mTLS fingerprint {fingerprint} is not authorized for shard {shard_id}"
                ));
            }
        }
    }

    {
        let known_shard = {
            let guard = state.shards.lock().await;
            guard.contains_key(&shard_id)
        };
        if !known_shard {
            timeout(
                WORKER_HANDSHAKE_TIMEOUT,
                transport::write_message(
                    &mut stream,
                    &RpcMessage::Error {
                        message: format!("unknown shard {shard_id}"),
                    },
                ),
            )
            .await
            .ok();
            return Err(anyhow!("worker connected for unknown shard {shard_id}"));
        }
    }

    let worker_id: WorkerId = state.next_worker_id.fetch_add(1, Ordering::SeqCst);

    // Reserve the shard for this handshake before sending RouterHello. This prevents a race where
    // two concurrent connections could both receive RouterHello and fight for shard ownership.
    #[derive(Debug)]
    enum ReservationFailure {
        UnknownShard,
        AlreadyHasWorker,
    }

    let reservation_failure = {
        let mut guard = state.shards.lock().await;
        match guard.get_mut(&shard_id) {
            None => Some(ReservationFailure::UnknownShard),
            Some(shard) => {
                if shard.worker.is_some() || shard.pending_worker.is_some() {
                    Some(ReservationFailure::AlreadyHasWorker)
                } else {
                    shard.pending_worker = Some(worker_id);
                    None
                }
            }
        }
    };

    if let Some(failure) = reservation_failure {
        let (message, err) = match failure {
            ReservationFailure::UnknownShard => (
                format!("unknown shard {shard_id}"),
                anyhow!("worker connected for unknown shard {shard_id}"),
            ),
            ReservationFailure::AlreadyHasWorker => (
                format!("shard {shard_id} already has a connected worker"),
                anyhow!("worker already connected for shard {shard_id}"),
            ),
        };
        timeout(
            WORKER_HANDSHAKE_TIMEOUT,
            transport::write_message(&mut stream, &RpcMessage::Error { message }),
        )
        .await
        .ok();
        return Err(err);
    }

    // Send RouterHello while the reservation is held.
    let send_hello = timeout(
        WORKER_HANDSHAKE_TIMEOUT,
        transport::write_message(
            &mut stream,
            &RpcMessage::RouterHello {
                worker_id,
                shard_id,
                revision: state.global_revision.load(Ordering::SeqCst),
                protocol_version: nova_remote_proto::PROTOCOL_VERSION,
            },
        ),
    )
    .await;

    let send_hello = match send_hello {
        Ok(res) => res,
        Err(_) => {
            let mut guard = state.shards.lock().await;
            if let Some(shard) = guard.get_mut(&shard_id) {
                if shard.pending_worker == Some(worker_id) {
                    shard.pending_worker = None;
                }
            }
            state.notify.notify_waiters();
            return Err(anyhow!("timed out sending RouterHello"));
        }
    };

    if let Err(err) = send_hello {
        let mut guard = state.shards.lock().await;
        if let Some(shard) = guard.get_mut(&shard_id) {
            if shard.pending_worker == Some(worker_id) {
                shard.pending_worker = None;
            }
        }
        state.notify.notify_waiters();
        return Err(err);
    }

    let (tx, rx) = mpsc::unbounded_channel::<WorkerRequest>();
    let handle = WorkerHandle {
        shard_id,
        worker_id,
        tx,
    };

    // Finalize the reservation now that RouterHello is on the wire.
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

    let cleanup_state = state.clone();
    tokio::spawn(async move {
        let _ = worker_connection_loop(worker_id, shard_id, stream, rx).await;
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
            let _ = refresh_handle.notify(RpcMessage::LoadFiles { revision, files });
        });
    }

    state.notify.notify_waiters();
    Ok(())
}

async fn worker_connection_loop(
    worker_id: WorkerId,
    shard_id: ShardId,
    mut stream: BoxedStream,
    mut rx: mpsc::UnboundedReceiver<WorkerRequest>,
) -> Result<()> {
    loop {
        let req = tokio::select! {
            req = rx.recv() => {
                let Some(req) = req else {
                    break;
                };
                req
            }
            // If the worker disconnects while no router RPCs are in flight, we'd otherwise never
            // notice because the protocol is request-driven (the worker doesn't send unsolicited
            // messages).
            //
            // That can leave `shard.worker` set even though the socket is closed, and because the
            // accept loop rejects a second worker connection for a shard, the router can get stuck
            // until some RPC attempt fails and triggers cleanup.
            //
            // Reading a single byte is enough to detect EOF. Seeing a byte here is unexpected and
            // indicates a protocol violation (legacy_v2 has no worker→router unsolicited messages),
            // so we log and drop the connection.
            res = stream.read_u8() => {
                match res {
                    Ok(byte) => {
                        warn!(
                            shard_id,
                            worker_id,
                            byte,
                            "received unexpected byte from worker while idle; closing connection"
                        );
                    }
                    Err(_) => {}
                }
                break;
            }
        };

        let message = req.message;

        let write_res = match timeout(
            WORKER_RPC_WRITE_TIMEOUT,
            transport::write_message(&mut stream, &message),
        )
        .await
        {
            Ok(res) => res
                .with_context(|| format!("write request to worker {worker_id} (shard {shard_id})")),
            Err(_) => Err(anyhow!(
                "timed out writing request to worker {worker_id} (shard {shard_id})"
            )),
        };
        if let Err(err) = write_res {
            if let Some(reply) = req.reply {
                let _ = reply.send(Err(err));
            }
            break;
        }

        if matches!(message, RpcMessage::Shutdown) {
            if let Some(reply) = req.reply {
                let _ = reply.send(Ok(RpcMessage::Shutdown));
            }
            break;
        }

        let read_res = match timeout(
            WORKER_RPC_READ_TIMEOUT,
            transport::read_message(&mut stream),
        )
        .await
        {
            Ok(res) => res.with_context(|| {
                format!("read response from worker {worker_id} (shard {shard_id})")
            }),
            Err(_) => Err(anyhow!(
                "timed out waiting for response from worker {worker_id} (shard {shard_id})"
            )),
        };
        match read_res {
            Ok(resp) => {
                if let Some(reply) = req.reply {
                    let _ = reply.send(Ok(resp));
                }
            }
            Err(err) => {
                if let Some(reply) = req.reply {
                    let _ = reply.send(Err(err));
                }
                break;
            }
        }
    }
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
    let mut reader = BufReader::new(reader);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf).await {
            Ok(0) => return,
            Ok(_) => {
                let line = String::from_utf8_lossy(&buf);
                let line = line.trim_end_matches(&['\r', '\n'][..]);
                info!(
                    target = "nova.worker.output",
                    shard_id,
                    stream = label,
                    line = %line,
                    "worker output"
                );
            }
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
        }
    }
}

fn index_for_files(shard_id: ShardId, mut files: Vec<FileText>) -> Vec<Symbol> {
    use nova_db::{FileId, NovaSemantic, SalsaDatabase, SourceRootId};

    files.sort_by(|a, b| a.path.cmp(&b.path));

    let db = SalsaDatabase::new();
    let root = SourceRootId::from_raw(shard_id);
    let mut file_ids = Vec::with_capacity(files.len());

    for (idx, file) in files.into_iter().enumerate() {
        let file_id = FileId::from_raw(idx as u32);
        db.set_file_exists(file_id, true);
        db.set_source_root(file_id, root);
        db.set_file_content(file_id, Arc::new(file.text));
        file_ids.push((file_id, file.path));
    }

    let snap = db.snapshot();
    let mut symbols = Vec::new();
    for (file_id, path) in file_ids {
        let summary = snap.symbol_summary(file_id);
        for name in &summary.names {
            symbols.push(Symbol {
                name: name.clone(),
                path: path.clone(),
            });
        }
    }

    symbols.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.path.cmp(&b.path)));
    symbols.dedup();
    symbols
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

#[derive(Debug, Clone)]
struct GlobalSymbolIndex {
    symbols: Vec<Symbol>,
    trigram: TrigramIndex,
    prefix1: Vec<Vec<u32>>,
}

impl Default for GlobalSymbolIndex {
    fn default() -> Self {
        Self {
            symbols: Vec::new(),
            trigram: TrigramIndexBuilder::new().build(),
            prefix1: vec![Vec::new(); 256],
        }
    }
}

impl GlobalSymbolIndex {
    fn new(symbols: Vec<Symbol>) -> Self {
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

        let mut scored = Vec::new();

        if query_bytes.len() < 3 {
            if let Some(b0) = query_first {
                let bucket = &self.prefix1[b0 as usize];
                if !bucket.is_empty() {
                    self.score_candidates(bucket.iter().copied(), &mut matcher, &mut scored);
                    return self.finish(scored, limit);
                }
            }

            let scan_limit = FALLBACK_SCAN_LIMIT.min(self.symbols.len());
            self.score_candidates(
                (0..scan_limit).map(|id| id as u32),
                &mut matcher,
                &mut scored,
            );
            return self.finish(scored, limit);
        }

        let mut candidates = self.trigram.candidates(query);
        if candidates.is_empty() {
            if let Some(b0) = query_first {
                let bucket = &self.prefix1[b0 as usize];
                if !bucket.is_empty() {
                    self.score_candidates(bucket.iter().copied(), &mut matcher, &mut scored);
                    return self.finish(scored, limit);
                }
            }

            let scan_limit = FALLBACK_SCAN_LIMIT.min(self.symbols.len());
            candidates = (0..scan_limit as u32).collect();
        }

        self.score_candidates(candidates.into_iter(), &mut matcher, &mut scored);
        self.finish(scored, limit)
    }

    fn score_candidates(
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

    fn finish(&self, mut scored: Vec<LocalScoredSymbol>, limit: usize) -> Vec<Symbol> {
        scored.sort_by(|a, b| {
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
        });

        scored
            .into_iter()
            .take(limit)
            .filter_map(|s| self.symbols.get(s.id as usize).cloned())
            .collect()
    }
}

#[derive(Debug, Clone)]
struct LocalScoredSymbol {
    id: u32,
    score: MatchScore,
}

async fn write_global_symbols(dst: &RwLock<GlobalSymbolIndex>, symbols: Vec<Symbol>) {
    let mut guard = dst.write().await;
    *guard = GlobalSymbolIndex::new(symbols);
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

        let index = GlobalSymbolIndex::new(symbols);
        let results = index.search("foo", 10);
        assert_eq!(results[0].name, "foobar");
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

        let index = GlobalSymbolIndex::new(symbols);
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

        let index = GlobalSymbolIndex::new(symbols);
        let results = index.search("Hash", 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "HashMap");
    }
}
