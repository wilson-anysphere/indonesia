use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use nova_remote_proto::{RpcMessage, ShardId, ShardIndex, WorkerStats};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

#[cfg(unix)]
use tokio::net::UnixStream;

#[cfg(windows)]
use tokio::net::windows::named_pipe::ClientOptions;

#[cfg(feature = "tls")]
mod tls;

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse()?;

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
            eprintln!("failed to load shard cache: {err:?}");
            None
        }
        Err(err) => {
            eprintln!("failed to join shard cache task: {err:?}");
            None
        }
    };

    write_message(
        &mut stream,
        &RpcMessage::WorkerHello {
            shard_id: args.shard_id,
            auth_token: args.auth_token.clone(),
            cached_index,
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

    let mut state = WorkerState::new(args.shard_id, args.cache_dir);
    state.run(&mut stream).await?;

    Ok(())
}

#[derive(Clone, Debug)]
struct Args {
    connect: ConnectAddr,
    shard_id: ShardId,
    cache_dir: PathBuf,
    auth_token: Option<String>,
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
}

impl Args {
    fn parse() -> Result<Self> {
        let mut connect = None;
        let mut shard_id = None;
        let mut cache_dir = None;
        let mut auth_token = None;
        let mut tls_ca_cert = None;
        let mut tls_domain = None;

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
                _ => return Err(anyhow!("unknown argument: {arg}")),
            }
        }

        let connect = connect.ok_or_else(|| anyhow!("--connect is required"))?;
        let shard_id = shard_id.ok_or_else(|| anyhow!("--shard-id is required"))?;
        let cache_dir = cache_dir.ok_or_else(|| anyhow!("--cache-dir is required"))?;

        #[cfg(not(feature = "tls"))]
        if tls_ca_cert.is_some() || tls_domain.is_some() {
            return Err(anyhow!(
                "TLS flags require building nova-worker with `--features tls`"
            ));
        }

        #[cfg(feature = "tls")]
        let tls = match (tls_ca_cert, tls_domain) {
            (Some(ca_cert), domain) => Some(TlsArgs {
                ca_cert,
                domain: domain.unwrap_or_else(|| "localhost".into()),
            }),
            (None, None) => None,
            _ => return Err(anyhow!("--tls-domain cannot be used without --tls-ca-cert")),
        };

        Ok(Self {
            connect: parse_connect_addr(&connect)?,
            shard_id,
            cache_dir,
            auth_token,
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
}

impl WorkerState {
    fn new(shard_id: ShardId, cache_dir: PathBuf) -> Self {
        Self {
            shard_id,
            cache_dir,
            revision: 0,
            index_generation: 0,
            files: HashMap::new(),
        }
    }

    async fn run(&mut self, stream: &mut BoxedStream) -> Result<()> {
        loop {
            let msg = match read_message(stream).await {
                Ok(msg) => msg,
                Err(err) => return Err(err),
            };

            match msg {
                RpcMessage::IndexShard { revision, files } => {
                    self.revision = revision;
                    self.files = files.into_iter().map(|f| (f.path, f.text)).collect();
                    let index = self.build_index().await?;
                    write_message(stream, &RpcMessage::ShardIndex(index)).await?;
                }
                RpcMessage::LoadFiles { revision, files } => {
                    self.revision = revision;
                    self.files = files.into_iter().map(|f| (f.path, f.text)).collect();
                    write_message(stream, &RpcMessage::Ack).await?;
                }
                RpcMessage::UpdateFile { revision, file } => {
                    self.revision = revision;
                    self.files.insert(file.path, file.text);
                    let index = self.build_index().await?;
                    write_message(stream, &RpcMessage::ShardIndex(index)).await?;
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

    async fn build_index(&mut self) -> Result<ShardIndex> {
        self.index_generation += 1;
        let mut files = std::collections::BTreeMap::new();
        for (path, text) in &self.files {
            files.insert(path.clone(), text.clone());
        }
        let index = nova_index::Index::new(files);
        let symbols = index
            .symbols()
            .iter()
            .map(|sym| nova_remote_proto::Symbol {
                name: sym.name.clone(),
                path: sym.file.clone(),
            })
            .collect();
        let index = ShardIndex {
            shard_id: self.shard_id,
            revision: self.revision,
            index_generation: self.index_generation,
            symbols,
        };

        let cache_dir = self.cache_dir.clone();
        let index_clone = index.clone();
        tokio::task::spawn_blocking(move || nova_cache::save_shard_index(&cache_dir, &index_clone))
            .await
            .context("join shard cache write")?
            .context("write shard cache")?;

        Ok(index)
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
    let mut buf = vec![0u8; len as usize];
    stream
        .read_exact(&mut buf)
        .await
        .context("read message payload")?;
    nova_remote_proto::decode_message(&buf)
}
