use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context};
use nova_fuzzy::{FuzzyMatcher, MatchScore, TrigramIndex, TrigramIndexBuilder};
use nova_remote_proto::{
    FileText, Revision, RpcMessage, ShardId, ShardIndex, Symbol, WorkerId, WorkerStats,
};
use nova_scheduler::{CancellationToken, Cancelled, Scheduler, SchedulerConfig};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot, watch, Mutex, Notify, RwLock};
use tokio::task::JoinHandle;
use tokio::time::{timeout, Duration};

#[cfg(unix)]
use tokio::net::UnixListener;

#[cfg(feature = "tls")]
pub mod tls;

pub type Result<T> = anyhow::Result<T>;

const WORKSPACE_SYMBOL_LIMIT: usize = 200;
const FALLBACK_SCAN_LIMIT: usize = 50_000;

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

#[derive(Clone, Debug)]
pub struct DistributedRouterConfig {
    pub listen_addr: ListenAddr,
    pub worker_command: PathBuf,
    pub cache_dir: PathBuf,
    pub auth_token: Option<String>,
    #[cfg(feature = "tls")]
    pub tls_client_cert_fingerprint_allowlist: TlsClientCertFingerprintAllowlist,
    /// If true, the router spawns `nova-worker` processes locally (multi-process mode).
    ///
    /// If false, workers are expected to be started externally (e.g. on remote machines)
    /// and connect to `listen_addr` via RPC.
    pub spawn_workers: bool,
}

#[cfg(feature = "tls")]
#[derive(Clone, Debug, Default)]
pub struct TlsClientCertFingerprintAllowlist {
    /// Fingerprints allowed to connect to any shard (handy for operators).
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

/// QueryRouter is the coordination point described in `docs/04-incremental-computation.md`.
///
/// In this MVP it is responsible for:
/// - partitioning work by source root (shard)
/// - delegating indexing to worker processes over a simple RPC transport
/// - aggregating shard indexes into a global workspace symbol view
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
            let task = self
                .scheduler
                .spawn_background_with_token(token.clone(), move |token| {
                    Cancelled::check(&token)?;
                    let symbols = index_for_files(&files);
                    Cancelled::check(&token)?;
                    Ok(symbols)
                });
            let symbols = match task.join().await {
                Ok(symbols) => symbols,
                Err(Cancelled) => return Ok(()),
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
                let symbols = index_for_files(&shard_files);
                Cancelled::check(&token)?;
                Ok(symbols)
            });
        let symbols = match task.join().await {
            Ok(symbols) => symbols,
            Err(Cancelled) => return Ok(()),
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
    global_symbols: RwLock<GlobalSymbolIndex>,
    shards: Mutex<HashMap<ShardId, ShardState>>,
    notify: Notify,
}

struct ShardState {
    root: PathBuf,
    worker: Option<WorkerHandle>,
    index: Option<ShardIndex>,
}

#[derive(Clone)]
struct WorkerHandle {
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
            .map_err(|_| anyhow!("worker {} disconnected", self.worker_id))?;
        rx.await.context("worker response channel closed")?
    }

    fn notify(&self, message: RpcMessage) -> Result<()> {
        self.tx
            .send(WorkerRequest {
                message,
                reply: None,
            })
            .map_err(|_| anyhow!("worker {} disconnected", self.worker_id))?;
        Ok(())
    }
}

impl DistributedRouter {
    async fn new(config: DistributedRouterConfig, layout: WorkspaceLayout) -> Result<Self> {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let mut max_cached_revision: Revision = 0;
        let mut shards = HashMap::new();
        for (idx, root) in layout.source_roots.iter().enumerate() {
            let cached = nova_cache::load_shard_index(&config.cache_dir, idx as ShardId)
                .ok()
                .flatten();
            if let Some(index) = cached.as_ref() {
                max_cached_revision = max_cached_revision.max(index.revision);
            }
            shards.insert(
                idx as ShardId,
                ShardState {
                    root: root.path.clone(),
                    worker: None,
                    index: cached,
                },
            );
        }

        let state = Arc::new(RouterState {
            config: config.clone(),
            layout,
            next_worker_id: AtomicU32::new(1),
            global_revision: AtomicU64::new(max_cached_revision),
            global_symbols: RwLock::new(GlobalSymbolIndex::default()),
            shards: Mutex::new(shards),
            notify: Notify::new(),
        });

        let cached_symbols = {
            let shard_guard = state.shards.lock().await;
            build_global_symbols(shard_guard.values().filter_map(|s| s.index.as_ref()))
        };
        write_global_symbols(&state.global_symbols, cached_symbols).await;

        let accept_state = state.clone();
        let accept_shutdown_rx = shutdown_rx.clone();
        let accept_task = tokio::spawn(async move {
            if let Err(err) = accept_loop(accept_state, accept_shutdown_rx).await {
                eprintln!("router accept loop terminated: {err:?}");
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
                RpcMessage::ShardIndex(index) => {
                    if index.shard_id != shard_id {
                        return Err(anyhow!(
                            "worker returned index for wrong shard {}",
                            index.shard_id
                        ));
                    }
                    self.update_shard_index(index).await;
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
            RpcMessage::ShardIndex(index) => {
                self.update_shard_index(index).await;
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

    async fn update_shard_index(&self, index: ShardIndex) {
        self.state
            .global_revision
            .fetch_max(index.revision, Ordering::SeqCst);
        let mut shard_guard = self.state.shards.lock().await;
        if let Some(shard) = shard_guard.get_mut(&index.shard_id) {
            shard.index = Some(index);
        }

        let symbols = build_global_symbols(shard_guard.values().filter_map(|s| s.index.as_ref()));
        drop(shard_guard);
        write_global_symbols(&self.state.global_symbols, symbols).await;
        self.state.notify.notify_waiters();
    }
}

async fn wait_for_worker(state: Arc<RouterState>, shard_id: ShardId) -> Result<WorkerHandle> {
    let deadline = Duration::from_secs(10);
    timeout(deadline, async {
        loop {
            if let Some(worker) = {
                let guard = state.shards.lock().await;
                guard.get(&shard_id).and_then(|s| s.worker.clone())
            } {
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
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create socket dir {parent:?}"))?;
    }

    let listener =
        UnixListener::bind(&path).with_context(|| format!("bind unix socket {path:?}"))?;

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    return Ok(());
                }
            }
            res = listener.accept() => {
                let (stream, _) = res.with_context(|| format!("accept unix socket {path:?}"))?;
                let boxed: BoxedStream = Box::new(stream);
                if let Err(err) = handle_new_connection(state.clone(), boxed, WorkerIdentity::Unauthenticated).await {
                    eprintln!("failed to handle worker connection: {err:?}");
                }
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
    use tokio::net::windows::named_pipe::ServerOptions;

    let name = normalize_pipe_name(&name);
    let mut server = ServerOptions::new()
        .first_pipe_instance(true)
        .create(&name)
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
                if let Err(err) = handle_new_connection(state.clone(), stream, WorkerIdentity::Unauthenticated).await {
                    eprintln!("failed to handle worker connection: {err:?}");
                }
                server = ServerOptions::new()
                    .create(&name)
                    .with_context(|| format!("create named pipe {name}"))?;
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
                let (boxed, identity): (BoxedStream, WorkerIdentity) = match &cfg {
                    TcpListenAddr::Plain(_) => (Box::new(stream), WorkerIdentity::Unauthenticated),
                    #[cfg(feature = "tls")]
                    TcpListenAddr::Tls { config, .. } => match tls::accept(stream, config.clone()).await {
                        Ok(accepted) => {
                            let identity = accepted
                                .client_cert_fingerprint
                                .map(WorkerIdentity::TlsClientCertFingerprint)
                                .unwrap_or(WorkerIdentity::Unauthenticated);
                            (Box::new(accepted.stream), identity)
                        }
                        Err(err) => {
                            eprintln!("tls accept failed from {peer_addr}: {err:?}");
                            continue;
                        }
                    },
                };
                if let Err(err) = handle_new_connection(state.clone(), boxed, identity).await {
                    eprintln!("failed to handle worker connection from {peer_addr}: {err:?}");
                }
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
    let hello = read_message(&mut stream).await?;
    let (shard_id, auth_token, cached_index) = match hello {
        RpcMessage::WorkerHello {
            shard_id,
            auth_token,
            cached_index,
        } => (shard_id, auth_token, cached_index),
        other => return Err(anyhow!("expected WorkerHello, got {other:?}")),
    };
    let has_cached_index = cached_index.is_some();

    if let Some(expected) = state.config.auth_token.as_ref() {
        if auth_token.as_deref() != Some(expected.as_str()) {
            write_message(
                &mut stream,
                &RpcMessage::Error {
                    message: "authentication failed".into(),
                },
            )
            .await
            .ok();
            return Err(anyhow!("worker authentication failed"));
        }
    }

    #[cfg(feature = "tls")]
    {
        if let Some(shard_allowlist) = state
            .config
            .tls_client_cert_fingerprint_allowlist
            .shards
            .get(&shard_id)
        {
            let Some(fingerprint) = identity.tls_client_cert_fingerprint() else {
                write_message(
                    &mut stream,
                    &RpcMessage::Error {
                        message: "mTLS client certificate required".into(),
                    },
                )
                .await
                .ok();
                return Err(anyhow!("shard {shard_id} requires mTLS client identity"));
            };

            let is_allowed = shard_allowlist
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(fingerprint))
                || state
                    .config
                    .tls_client_cert_fingerprint_allowlist
                    .global
                    .iter()
                    .any(|allowed| allowed.eq_ignore_ascii_case(fingerprint));

            if !is_allowed {
                write_message(
                    &mut stream,
                    &RpcMessage::Error {
                        message: "shard authorization failed".into(),
                    },
                )
                .await
                .ok();
                return Err(anyhow!(
                    "worker mTLS fingerprint {fingerprint} is not authorized for shard {shard_id}"
                ));
            }
        }
    }

    #[derive(Debug)]
    enum ShardCheckFailure {
        UnknownShard,
        AlreadyHasWorker,
    }
    let shard_check = {
        let guard = state.shards.lock().await;
        match guard.get(&shard_id) {
            None => Some(ShardCheckFailure::UnknownShard),
            Some(shard) if shard.worker.is_some() => Some(ShardCheckFailure::AlreadyHasWorker),
            Some(_) => None,
        }
    };
    if let Some(failure) = shard_check {
        let (message, err) = match failure {
            ShardCheckFailure::UnknownShard => (
                format!("unknown shard {shard_id}"),
                anyhow!("worker connected for unknown shard {shard_id}"),
            ),
            ShardCheckFailure::AlreadyHasWorker => (
                format!("shard {shard_id} already has a connected worker"),
                anyhow!("worker already connected for shard {shard_id}"),
            ),
        };
        write_message(&mut stream, &RpcMessage::Error { message })
            .await
            .ok();
        return Err(err);
    }

    let worker_id: WorkerId = state.next_worker_id.fetch_add(1, Ordering::SeqCst);
    write_message(
        &mut stream,
        &RpcMessage::RouterHello {
            worker_id,
            shard_id,
            revision: state.global_revision.load(Ordering::SeqCst),
            protocol_version: nova_remote_proto::PROTOCOL_VERSION,
        },
    )
    .await?;

    let (tx, rx) = mpsc::unbounded_channel::<WorkerRequest>();
    let handle = WorkerHandle { worker_id, tx };

    if let Some(index) = cached_index.as_ref() {
        state
            .global_revision
            .fetch_max(index.revision, Ordering::SeqCst);
    }

    {
        let mut guard = state.shards.lock().await;
        let shard = guard
            .get_mut(&shard_id)
            .ok_or_else(|| anyhow!("worker connected for unknown shard {shard_id}"))?;
        shard.worker = Some(handle.clone());
        if cached_index.is_some() {
            shard.index = cached_index;
        }
    }

    let cleanup_state = state.clone();
    tokio::spawn(async move {
        let _ = worker_connection_loop(stream, rx).await;
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

    let symbols = {
        let shard_guard = state.shards.lock().await;
        build_global_symbols(shard_guard.values().filter_map(|s| s.index.as_ref()))
    };
    write_global_symbols(&state.global_symbols, symbols).await;

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
                    eprintln!("failed to load shard files for worker restart: {err:?}");
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
    mut stream: BoxedStream,
    mut rx: mpsc::UnboundedReceiver<WorkerRequest>,
) -> Result<()> {
    while let Some(req) = rx.recv().await {
        let message = req.message;

        if let Err(err) = write_message(&mut stream, &message).await {
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

        match read_message(&mut stream).await {
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

async fn worker_supervisor_loop(
    state: Arc<RouterState>,
    shard_id: ShardId,
    shutdown_rx: &mut watch::Receiver<bool>,
) {
    let connect_arg = state.config.listen_addr.as_worker_connect_arg();

    loop {
        if *shutdown_rx.borrow() {
            return;
        }

        let mut cmd = Command::new(&state.config.worker_command);
        cmd.kill_on_drop(true);
        cmd.arg("--connect")
            .arg(&connect_arg)
            .arg("--shard-id")
            .arg(shard_id.to_string())
            .arg("--cache-dir")
            .arg(&state.config.cache_dir);

        if let Some(token) = state.config.auth_token.as_ref() {
            cmd.arg("--auth-token").arg(token);
        }

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(err) => {
                eprintln!("failed to spawn worker for shard {shard_id}: {err:?}");
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            }
        };

        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    return;
                }
            }
            _ = child.wait() => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

fn index_for_files(files: &[FileText]) -> Vec<Symbol> {
    let mut map = std::collections::BTreeMap::new();
    for file in files {
        map.insert(file.path.clone(), file.text.clone());
    }
    let index = nova_index::Index::new(map);
    index
        .symbols()
        .iter()
        .map(|sym| Symbol {
            name: sym.name.clone(),
            path: sym.file.clone(),
        })
        .collect()
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
        out: &mut Vec<ScoredSymbol>,
    ) {
        for id in ids {
            let Some(sym) = self.symbols.get(id as usize) else {
                continue;
            };
            if let Some(score) = matcher.score(&sym.name) {
                out.push(ScoredSymbol { id, score });
            }
        }
    }

    fn finish(&self, mut scored: Vec<ScoredSymbol>, limit: usize) -> Vec<Symbol> {
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
struct ScoredSymbol {
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

async fn write_message(stream: &mut (impl AsyncWrite + Unpin), message: &RpcMessage) -> Result<()> {
    let payload = nova_remote_proto::encode_message(message)?;
    let len: u32 = payload
        .len()
        .try_into()
        .map_err(|_| anyhow!("message too large"))?;

    stream
        .write_u32_le(len)
        .await
        .context("write message len")?;
    stream
        .write_all(&payload)
        .await
        .context("write message payload")?;
    stream.flush().await.context("flush message")?;
    Ok(())
}

async fn read_message(stream: &mut (impl AsyncRead + Unpin)) -> Result<RpcMessage> {
    let len = stream.read_u32_le().await.context("read message len")?;
    let mut buf = vec![0u8; len as usize];
    stream
        .read_exact(&mut buf)
        .await
        .context("read message payload")?;
    nova_remote_proto::decode_message(&buf)
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
