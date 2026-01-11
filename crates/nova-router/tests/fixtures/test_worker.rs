use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use nova_remote_proto::{RpcMessage, ShardId, WorkerStats};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

#[cfg(unix)]
use tokio::net::UnixStream;

#[cfg(windows)]
use tokio::net::windows::named_pipe::ClientOptions;

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse()?;
    std::fs::create_dir_all(&args.cache_dir).context("create cache dir")?;

    let cfg = TestWorkerConfig::load(&args.cache_dir);
    let attempt = record_attempt(&args.cache_dir, args.shard_id)?;

    println!("test worker shard {} attempt {}", args.shard_id, attempt);
    eprintln!("test worker shard {} attempt {}", args.shard_id, attempt);

    if attempt <= cfg.fail_attempts {
        eprintln!(
            "test worker shard {} failing attempt {} (configured fail_attempts={})",
            args.shard_id, attempt, cfg.fail_attempts
        );
        std::process::exit(1);
    }

    if attempt <= cfg.connect_delay_attempts && cfg.connect_delay_ms > 0 {
        tokio::time::sleep(Duration::from_millis(cfg.connect_delay_ms)).await;
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
    };

    write_message(
        &mut stream,
        &RpcMessage::WorkerHello {
            shard_id: args.shard_id,
            auth_token: args.auth_token.clone(),
            has_cached_index: false,
        },
    )
    .await?;

    let ack = read_message(&mut stream).await?;
    match ack {
        RpcMessage::RouterHello {
            shard_id,
            protocol_version,
            ..
        } if shard_id == args.shard_id
            && protocol_version == nova_remote_proto::PROTOCOL_VERSION => {}
        other => return Err(anyhow!("unexpected router hello: {other:?}")),
    }

    if attempt <= cfg.exit_after_handshake_attempts {
        if cfg.exit_after_handshake_delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(cfg.exit_after_handshake_delay_ms)).await;
        }
        eprintln!(
            "test worker shard {} exiting after handshake (attempt {})",
            args.shard_id, attempt
        );
        return Ok(());
    }

    let mut state = WorkerState::new(args.shard_id);
    loop {
        let msg = read_message(&mut stream).await?;
        match msg {
            RpcMessage::LoadFiles { revision, files } => {
                state.revision = revision;
                state.file_count = files.len().try_into().unwrap_or(u32::MAX);
                write_message(&mut stream, &RpcMessage::Ack).await?;
            }
            RpcMessage::IndexShard { revision, files } => {
                state.revision = revision;
                state.file_count = files.len().try_into().unwrap_or(u32::MAX);
                state.index_generation = state.index_generation.saturating_add(1);
                write_message(
                    &mut stream,
                    &RpcMessage::ShardIndexInfo(nova_remote_proto::ShardIndexInfo {
                        shard_id: state.shard_id,
                        revision: state.revision,
                        index_generation: state.index_generation,
                        symbol_count: 0,
                    }),
                )
                .await?;
            }
            RpcMessage::UpdateFile { revision, .. } => {
                state.revision = revision;
                state.file_count = state.file_count.max(1);
                state.index_generation = state.index_generation.saturating_add(1);
                write_message(
                    &mut stream,
                    &RpcMessage::ShardIndexInfo(nova_remote_proto::ShardIndexInfo {
                        shard_id: state.shard_id,
                        revision: state.revision,
                        index_generation: state.index_generation,
                        symbol_count: 0,
                    }),
                )
                .await?;
            }
            RpcMessage::GetWorkerStats => {
                write_message(&mut stream, &RpcMessage::WorkerStats(state.stats())).await?;
            }
            RpcMessage::SearchSymbols { .. } => {
                write_message(
                    &mut stream,
                    &RpcMessage::SearchSymbolsResult { items: Vec::new() },
                )
                .await?;
            }
            RpcMessage::Shutdown => return Ok(()),
            _ => write_message(&mut stream, &RpcMessage::Ack).await?,
        }
    }
}

struct WorkerState {
    shard_id: ShardId,
    revision: u64,
    index_generation: u64,
    file_count: u32,
}

impl WorkerState {
    fn new(shard_id: ShardId) -> Self {
        Self {
            shard_id,
            revision: 0,
            index_generation: 0,
            file_count: 0,
        }
    }

    fn stats(&self) -> WorkerStats {
        WorkerStats {
            shard_id: self.shard_id,
            revision: self.revision,
            index_generation: self.index_generation,
            file_count: self.file_count,
        }
    }
}

#[derive(Clone, Debug)]
struct Args {
    connect: ConnectAddr,
    shard_id: ShardId,
    cache_dir: PathBuf,
    auth_token: Option<String>,
}

#[derive(Clone, Debug)]
enum ConnectAddr {
    #[cfg(unix)]
    Unix(PathBuf),
    #[cfg(windows)]
    NamedPipe(String),
    Tcp(SocketAddr),
}

impl Args {
    fn parse() -> Result<Self> {
        let mut connect = None;
        let mut shard_id = None;
        let mut cache_dir = None;
        let mut auth_token = None;
        let mut auth_token_file: Option<PathBuf> = None;
        let mut auth_token_env: Option<String> = None;

        let mut iter = std::env::args().skip(1);
        while let Some(arg) = iter.next() {
            match arg.as_str() {
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
                other => return Err(anyhow!("unknown argument: {other}")),
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

        Ok(Self {
            connect: parse_connect_addr(&connect)?,
            shard_id,
            cache_dir,
            auth_token,
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

#[derive(Default)]
struct TestWorkerConfig {
    fail_attempts: u32,
    connect_delay_ms: u64,
    connect_delay_attempts: u32,
    exit_after_handshake_attempts: u32,
    exit_after_handshake_delay_ms: u64,
}

impl TestWorkerConfig {
    fn load(cache_dir: &Path) -> Self {
        let path = cache_dir.join("nova-router-test-worker.conf");
        let contents = match std::fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(_) => return Self::default(),
        };

        let mut cfg = Self::default();
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            let Some((key, value)) = line.split_once('=') else {
                continue;
            };

            match key.trim() {
                "fail_attempts" => {
                    cfg.fail_attempts = value.trim().parse().unwrap_or(cfg.fail_attempts);
                }
                "connect_delay_ms" => {
                    cfg.connect_delay_ms = value.trim().parse().unwrap_or(cfg.connect_delay_ms);
                }
                "connect_delay_attempts" => {
                    cfg.connect_delay_attempts =
                        value.trim().parse().unwrap_or(cfg.connect_delay_attempts);
                }
                "exit_after_handshake_attempts" => {
                    cfg.exit_after_handshake_attempts = value
                        .trim()
                        .parse()
                        .unwrap_or(cfg.exit_after_handshake_attempts);
                }
                "exit_after_handshake_delay_ms" => {
                    cfg.exit_after_handshake_delay_ms = value
                        .trim()
                        .parse()
                        .unwrap_or(cfg.exit_after_handshake_delay_ms);
                }
                _ => {}
            }
        }

        cfg
    }
}

fn record_attempt(cache_dir: &Path, shard_id: ShardId) -> Result<u32> {
    let counter_path = cache_dir.join(format!("attempts-shard{shard_id}.count"));
    let current = std::fs::read_to_string(&counter_path)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(0);
    let next = current.saturating_add(1);
    std::fs::write(&counter_path, next.to_string()).context("write attempt counter")?;

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("read system time")?
        .as_millis();
    let log_path = cache_dir.join(format!("attempts-shard{shard_id}.log"));
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("open attempt log {log_path:?}"))?;
    use std::io::Write as _;
    writeln!(file, "{now_ms}").context("append attempt log")?;

    Ok(next)
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
    let mut buf = vec![0u8; len as usize];
    stream
        .read_exact(&mut buf)
        .await
        .context("read message payload")?;
    nova_remote_proto::decode_message(&buf)
}
