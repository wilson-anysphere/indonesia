use std::collections::HashMap;
use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use nova_bugreport::{install_panic_hook, PanicHookConfig};
use nova_config::{init_tracing_with_config, NovaConfig};
use nova_fuzzy::{FuzzyMatcher, MatchKind, MatchScore, TrigramIndex, TrigramIndexBuilder};
use nova_remote_proto::{
    RpcMessage, ScoredSymbol, ShardId, ShardIndex, ShardIndexInfo, SymbolRankKey, WorkerStats,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{error, info, warn};

#[cfg(unix)]
use tokio::net::UnixStream;

#[cfg(windows)]
use tokio::net::windows::named_pipe::ClientOptions;

#[cfg(feature = "tls")]
mod tls;

const FALLBACK_SCAN_LIMIT: usize = 50_000;

#[tokio::main]
async fn main() -> Result<()> {
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

    let mut stream: BoxedStream = match args.connect {
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
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
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

    let has_cached_index = cached_index.is_some();
    write_message(
        &mut stream,
        &RpcMessage::WorkerHello {
            shard_id: args.shard_id,
            auth_token: args.auth_token.clone(),
            has_cached_index,
        },
    )
    .await?;

    let ack = read_message(&mut stream)
        .await
        .with_context(|| format!("read RouterHello for shard {}", args.shard_id))?;
    let (worker_id, shard_id, revision, protocol_version) = match ack {
        RpcMessage::RouterHello {
            worker_id,
            shard_id,
            revision,
            protocol_version,
            ..
        } => (worker_id, shard_id, revision, protocol_version),
        other => return Err(anyhow!("unexpected router hello: {other:?}")),
    };

    if shard_id != args.shard_id {
        return Err(anyhow!(
            "router hello shard mismatch: expected {}, got {}",
            args.shard_id,
            shard_id
        ));
    }

    if protocol_version != nova_remote_proto::PROTOCOL_VERSION {
        return Err(anyhow!(
            "router hello protocol version mismatch: expected {}, got {}",
            nova_remote_proto::PROTOCOL_VERSION,
            protocol_version
        ));
    }

    span.record("worker_id", worker_id);
    info!(worker_id, revision, protocol_version, "connected to router");

    let mut state = WorkerState::new(args.shard_id, args.cache_dir, cached_index);
    if let Err(err) = state.run(&mut stream).await {
        error!(error = ?err, "worker terminated with error");
        return Err(err);
    }
    info!("worker shutdown");

    Ok(())
}

#[derive(Clone, Debug)]
struct Args {
    connect: ConnectAddr,
    shard_id: ShardId,
    cache_dir: PathBuf,
    auth_token: Option<String>,
    allow_insecure: bool,
    #[cfg(feature = "tls")]
    tls: Option<TlsArgs>,
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
        let mut tls_ca_cert = None;
        let mut tls_domain = None;
        let mut tls_client_cert = None;
        let mut tls_client_key = None;

        let mut iter = std::env::args().skip(1);
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--allow-insecure" => allow_insecure = true,
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
    files: HashMap<String, String>,
    symbol_index: Option<ShardSymbolSearchIndex>,
}

impl WorkerState {
    fn new(shard_id: ShardId, cache_dir: PathBuf, cached_index: Option<ShardIndex>) -> Self {
        let (index_generation, symbol_index) = match cached_index {
            Some(index) => {
                let index_generation = index.index_generation;
                let index = Arc::new(index);
                (index_generation, Some(ShardSymbolSearchIndex::new(index)))
            }
            None => (0, None),
        };

        Self {
            shard_id,
            cache_dir,
            revision: 0,
            index_generation,
            files: HashMap::new(),
            symbol_index,
        }
    }

    async fn run(&mut self, stream: &mut BoxedStream) -> Result<()> {
        loop {
            let msg = read_message(stream)
                .await
                .with_context(|| format!("read message from router (shard {})", self.shard_id))?;

            match msg {
                RpcMessage::IndexShard { revision, files } => {
                    self.revision = revision;
                    self.files = files.into_iter().map(|f| (f.path, f.text)).collect();
                    let info = self.build_index().await?;
                    write_message(stream, &RpcMessage::ShardIndexInfo(info)).await?;
                }
                RpcMessage::LoadFiles { revision, files } => {
                    self.revision = revision;
                    self.files = files.into_iter().map(|f| (f.path, f.text)).collect();
                    write_message(stream, &RpcMessage::Ack).await?;
                }
                RpcMessage::UpdateFile { revision, file } => {
                    self.revision = revision;
                    self.files.insert(file.path, file.text);
                    let info = self.build_index().await?;
                    write_message(stream, &RpcMessage::ShardIndexInfo(info)).await?;
                }
                RpcMessage::GetWorkerStats => {
                    let stats = WorkerStats {
                        shard_id: self.shard_id,
                        revision: self.revision,
                        index_generation: self.index_generation,
                        file_count: self.files.len().try_into().unwrap_or(u32::MAX),
                    };
                    write_message(stream, &RpcMessage::WorkerStats(stats)).await?;
                }
                RpcMessage::SearchSymbols { query, limit } => {
                    let limit = usize::try_from(limit).unwrap_or(usize::MAX);
                    let items = self
                        .symbol_index
                        .as_ref()
                        .map(|index| index.search(&query, limit))
                        .unwrap_or_default();
                    write_message(stream, &RpcMessage::SearchSymbolsResult { items }).await?;
                }
                RpcMessage::Shutdown => return Ok(()),
                other => {
                    write_message(
                        stream,
                        &RpcMessage::Error {
                            message: format!("unexpected message: {other:?}"),
                        },
                    )
                    .await?;
                }
            }
        }
    }

    async fn build_index(&mut self) -> Result<ShardIndexInfo> {
        self.index_generation += 1;
        let mut files = std::collections::BTreeMap::new();
        for (path, text) in &self.files {
            files.insert(path.clone(), text.clone());
        }
        let index = nova_index::Index::new(files);
        let mut symbols: Vec<nova_remote_proto::Symbol> = index
            .symbols()
            .iter()
            .map(|sym| nova_remote_proto::Symbol {
                name: sym.name.clone(),
                path: sym.file.clone(),
            })
            .collect();

        symbols.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.path.cmp(&b.path)));
        symbols.dedup();

        let symbol_count = symbols.len();
        let index = Arc::new(ShardIndex {
            shard_id: self.shard_id,
            revision: self.revision,
            index_generation: self.index_generation,
            symbols,
        });

        self.symbol_index = Some(ShardSymbolSearchIndex::new(index.clone()));

        let cache_dir = self.cache_dir.clone();
        let index_clone = index.clone();
        tokio::task::spawn_blocking(move || {
            nova_cache::save_shard_index(&cache_dir, index_clone.as_ref())
        })
        .await
        .context("join shard cache write")?
        .context("write shard cache")?;

        Ok(ShardIndexInfo {
            shard_id: self.shard_id,
            revision: self.revision,
            index_generation: self.index_generation,
            symbol_count: symbol_count.try_into().unwrap_or(u32::MAX),
        })
    }
}

struct ShardSymbolSearchIndex {
    index: Arc<ShardIndex>,
    trigram: TrigramIndex,
    prefix1: Vec<Vec<u32>>,
}

impl ShardSymbolSearchIndex {
    fn new(index: Arc<ShardIndex>) -> Self {
        let mut prefix1: Vec<Vec<u32>> = vec![Vec::new(); 256];
        let mut builder = TrigramIndexBuilder::new();

        for (id, sym) in index.symbols.iter().enumerate() {
            let id_u32: u32 = id
                .try_into()
                .unwrap_or_else(|_| panic!("symbol index too large: {id}"));

            builder.insert(id_u32, &sym.name);

            if let Some(&b0) = sym.name.as_bytes().first() {
                prefix1[b0.to_ascii_lowercase() as usize].push(id_u32);
            }
        }

        Self {
            index,
            trigram: builder.build(),
            prefix1,
        }
    }

    fn search(&self, query: &str, limit: usize) -> Vec<ScoredSymbol> {
        if limit == 0 || self.index.symbols.is_empty() {
            return Vec::new();
        }

        if query.is_empty() {
            return self
                .index
                .symbols
                .iter()
                .take(limit)
                .map(|sym| ScoredSymbol {
                    name: sym.name.clone(),
                    path: sym.path.clone(),
                    rank_key: SymbolRankKey {
                        kind_rank: 0,
                        score: 0,
                    },
                })
                .collect();
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

            let scan_limit = FALLBACK_SCAN_LIMIT.min(self.index.symbols.len());
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

            let scan_limit = FALLBACK_SCAN_LIMIT.min(self.index.symbols.len());
            candidates = (0..scan_limit as u32).collect();
        }

        self.score_candidates(candidates.into_iter(), &mut matcher, &mut scored);
        self.finish(scored, limit)
    }

    fn score_candidates(
        &self,
        ids: impl IntoIterator<Item = u32>,
        matcher: &mut FuzzyMatcher,
        out: &mut Vec<ScoredSymbolInternal>,
    ) {
        for id in ids {
            let Some(sym) = self.index.symbols.get(id as usize) else {
                continue;
            };
            if let Some(score) = matcher.score(&sym.name) {
                out.push(ScoredSymbolInternal { id, score });
            }
        }
    }

    fn finish(&self, mut scored: Vec<ScoredSymbolInternal>, limit: usize) -> Vec<ScoredSymbol> {
        scored.sort_by(|a, b| {
            b.score.rank_key().cmp(&a.score.rank_key()).then_with(|| {
                let a_sym = &self.index.symbols[a.id as usize];
                let b_sym = &self.index.symbols[b.id as usize];
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
            .filter_map(|s| {
                let sym = self.index.symbols.get(s.id as usize)?;
                Some(ScoredSymbol {
                    name: sym.name.clone(),
                    path: sym.path.clone(),
                    rank_key: match_score_rank_key(s.score),
                })
            })
            .collect()
    }
}

#[derive(Debug, Clone)]
struct ScoredSymbolInternal {
    id: u32,
    score: MatchScore,
}

fn match_score_rank_key(score: MatchScore) -> SymbolRankKey {
    let kind_rank = match score.kind {
        MatchKind::Prefix => 2,
        MatchKind::Fuzzy => 1,
    };
    SymbolRankKey {
        kind_rank,
        score: score.score,
    }
}

type BoxedStream = Box<dyn AsyncReadWrite>;

trait AsyncReadWrite: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T> AsyncReadWrite for T where T: AsyncRead + AsyncWrite + Unpin + Send {}

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
    let len_usize = len as usize;
    if len_usize > nova_remote_proto::MAX_MESSAGE_BYTES {
        return Err(anyhow!(
            "rpc payload too large: {len_usize} bytes (max {})",
            nova_remote_proto::MAX_MESSAGE_BYTES
        ));
    }
    // Use fallible reservation so allocation failure surfaces as an error rather than aborting the
    // process.
    let mut buf = Vec::new();
    buf.try_reserve_exact(len_usize)
        .context("allocate message buffer")?;
    buf.resize(len_usize, 0);
    stream
        .read_exact(&mut buf)
        .await
        .context("read message payload")?;
    nova_remote_proto::decode_message(&buf).context("decode message")
}
