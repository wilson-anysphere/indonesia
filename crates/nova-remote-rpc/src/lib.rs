use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::sync::{mpsc, oneshot, Mutex};

pub type RequestId = u64;

pub const PROTOCOL_VERSION: u32 = 3;

const TRACE_TARGET: &str = "nova.remote_rpc";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum PayloadKind {
    HandshakeHello = 1,
    HandshakeWelcome = 2,
    Request = 3,
    Response = 4,
    Error = 5,
}

impl PayloadKind {
    fn from_wire(value: u8) -> Result<Self> {
        Ok(match value {
            1 => Self::HandshakeHello,
            2 => Self::HandshakeWelcome,
            3 => Self::Request,
            4 => Self::Response,
            5 => Self::Error,
            other => return Err(anyhow!("unknown payload kind: {other}")),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Compression {
    None,
    Zstd,
}

#[derive(Clone, Serialize, Deserialize)]
struct ClientHello {
    supported_versions: Vec<u32>,
    supported_compressions: Vec<Compression>,
    auth_token: Option<String>,
}

impl fmt::Debug for ClientHello {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientHello")
            .field("supported_versions", &self.supported_versions)
            .field("supported_compressions", &self.supported_compressions)
            .field(
                "auth_token",
                &self.auth_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ServerWelcome {
    version: u32,
    compression: Compression,
}

#[derive(Debug, Clone)]
pub struct Negotiated {
    pub version: u32,
    pub compression: Compression,
}

#[derive(Clone)]
pub struct ClientConfig {
    pub supported_versions: Vec<u32>,
    pub supported_compressions: Vec<Compression>,
    pub compression_threshold: usize,
    pub auth_token: Option<String>,
}

impl fmt::Debug for ClientConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientConfig")
            .field("supported_versions", &self.supported_versions)
            .field("supported_compressions", &self.supported_compressions)
            .field("compression_threshold", &self.compression_threshold)
            .field(
                "auth_token",
                &self.auth_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            supported_versions: vec![PROTOCOL_VERSION],
            supported_compressions: vec![Compression::None, Compression::Zstd],
            compression_threshold: 1024,
            auth_token: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub supported_versions: Vec<u32>,
    pub supported_compressions: Vec<Compression>,
    pub compression_threshold: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            supported_versions: vec![PROTOCOL_VERSION],
            supported_compressions: vec![Compression::None, Compression::Zstd],
            compression_threshold: 1024,
        }
    }
}

#[derive(Debug, Clone)]
pub struct IncomingRequest {
    pub request_id: RequestId,
    pub payload: Vec<u8>,
}

pub struct Client {
    inner: Arc<Inner>,
}

pub struct Server {
    inner: Arc<Inner>,
    incoming: mpsc::UnboundedReceiver<IncomingRequest>,
    pub peer_auth_token: Option<String>,
}

struct Inner {
    writer: Mutex<WriteHalf<BoxedStream>>,
    pending: Mutex<HashMap<RequestId, oneshot::Sender<Vec<u8>>>>,
    negotiated: Negotiated,
    compression_threshold: usize,
    next_request_id: AtomicU64,
}

type BoxedStream = Box<dyn AsyncReadWrite>;

trait AsyncReadWrite: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T> AsyncReadWrite for T where T: AsyncRead + AsyncWrite + Unpin + Send {}

impl Client {
    pub async fn connect<S>(stream: S, config: ClientConfig) -> Result<Self>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let mut stream: BoxedStream = Box::new(stream);
        let negotiated = client_handshake(&mut stream, &config).await?;
        let compression_threshold = config.compression_threshold;
        let (reader, writer) = tokio::io::split(stream);

        let inner = Arc::new(Inner {
            writer: Mutex::new(writer),
            pending: Mutex::new(HashMap::new()),
            negotiated,
            compression_threshold,
            next_request_id: AtomicU64::new(1),
        });

        let inner_clone = inner.clone();
        tokio::spawn(async move { read_loop(reader, inner_clone, None).await });

        Ok(Self { inner })
    }

    pub fn negotiated(&self) -> &Negotiated {
        &self.inner.negotiated
    }

    pub async fn call(&self, payload: Vec<u8>) -> Result<Vec<u8>> {
        let request_id = self.inner.next_request_id.fetch_add(1, Ordering::Relaxed);

        let latency_enabled = tracing::enabled!(target: TRACE_TARGET, level: tracing::Level::DEBUG);
        let start = latency_enabled.then(Instant::now);

        let span = tracing::debug_span!(
            target: TRACE_TARGET,
            "call",
            request_id,
            payload_bytes = payload.len()
        );
        let _guard = span.enter();

        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.inner.pending.lock().await;
            pending.insert(request_id, tx);
        }

        if let Err(err) = self
            .inner
            .send_packet(request_id, PayloadKind::Request, payload)
            .await
        {
            let mut pending = self.inner.pending.lock().await;
            pending.remove(&request_id);
            return Err(err);
        }

        let response = rx
            .await
            .map_err(|_| anyhow!("connection closed while waiting for response"))?;

        if let Some(start) = start {
            let elapsed = start.elapsed();
            tracing::debug!(
                target: TRACE_TARGET,
                event = "call_complete",
                request_id,
                latency_ms = elapsed.as_secs_f64() * 1000.0,
                response_bytes = response.len()
            );
        }

        Ok(response)
    }
}

impl Server {
    pub async fn accept<S>(stream: S, config: ServerConfig) -> Result<Self>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let mut stream: BoxedStream = Box::new(stream);
        let (negotiated, peer_auth_token) = server_handshake(&mut stream, &config).await?;
        let compression_threshold = config.compression_threshold;
        let (reader, writer) = tokio::io::split(stream);
        let (incoming_tx, incoming) = mpsc::unbounded_channel();

        let inner = Arc::new(Inner {
            writer: Mutex::new(writer),
            pending: Mutex::new(HashMap::new()),
            negotiated,
            compression_threshold,
            next_request_id: AtomicU64::new(1),
        });

        let inner_clone = inner.clone();
        tokio::spawn(async move { read_loop(reader, inner_clone, Some(incoming_tx)).await });

        Ok(Self {
            inner,
            incoming,
            peer_auth_token,
        })
    }

    pub fn negotiated(&self) -> &Negotiated {
        &self.inner.negotiated
    }

    pub async fn recv_request(&mut self) -> Option<IncomingRequest> {
        self.incoming.recv().await
    }

    pub async fn respond(&self, request_id: RequestId, payload: Vec<u8>) -> Result<()> {
        self.inner
            .send_packet(request_id, PayloadKind::Response, payload)
            .await
    }
}

impl Inner {
    async fn send_packet(
        &self,
        request_id: RequestId,
        kind: PayloadKind,
        payload: Vec<u8>,
    ) -> Result<()> {
        let (wire_payload, compressed, uncompressed_len) =
            maybe_compress(&self.negotiated, self.compression_threshold, payload)?;
        let wire_len: u32 = wire_payload
            .len()
            .try_into()
            .map_err(|_| anyhow!("payload too large"))?;

        tracing::trace!(
            target: TRACE_TARGET,
            direction = "send",
            request_id,
            kind = ?kind,
            compressed,
            bytes = wire_len,
            uncompressed_bytes = uncompressed_len
        );

        let mut writer = self.writer.lock().await;
        writer
            .write_u64_le(request_id)
            .await
            .context("write request id")?;
        writer.write_u8(kind as u8).await.context("write kind")?;
        writer
            .write_u8(compressed as u8)
            .await
            .context("write compressed flag")?;
        writer
            .write_u32_le(wire_len)
            .await
            .context("write payload len")?;
        writer
            .write_u32_le(uncompressed_len)
            .await
            .context("write uncompressed len")?;
        writer
            .write_all(&wire_payload)
            .await
            .context("write payload")?;
        writer.flush().await.context("flush payload")?;
        Ok(())
    }
}

async fn client_handshake(stream: &mut BoxedStream, config: &ClientConfig) -> Result<Negotiated> {
    tracing::debug!(
        target: TRACE_TARGET,
        event = "handshake_start",
        role = "client",
        supported_versions = ?config.supported_versions,
        supported_compressions = ?config.supported_compressions,
        auth_present = config.auth_token.is_some()
    );

    let hello = ClientHello {
        supported_versions: config.supported_versions.clone(),
        supported_compressions: config.supported_compressions.clone(),
        auth_token: config.auth_token.clone(),
    };
    let payload = bincode::serialize(&hello).context("encode client hello")?;
    let payload_len: u32 = payload
        .len()
        .try_into()
        .map_err(|_| anyhow!("handshake payload too large"))?;
    write_raw_packet(
        stream,
        0,
        PayloadKind::HandshakeHello,
        false,
        payload_len,
        payload,
    )
    .await?;

    let packet = read_raw_packet(stream).await?;
    if packet.kind != PayloadKind::HandshakeWelcome || packet.request_id != 0 {
        return Err(anyhow!(
            "unexpected handshake packet: kind={:?} request_id={}",
            packet.kind,
            packet.request_id
        ));
    }

    let welcome: ServerWelcome =
        bincode::deserialize(&packet.payload).context("decode server welcome")?;
    if !config.supported_versions.contains(&welcome.version) {
        return Err(anyhow!(
            "server negotiated unsupported protocol version {}",
            welcome.version
        ));
    }
    if !config.supported_compressions.contains(&welcome.compression) {
        return Err(anyhow!(
            "server negotiated unsupported compression {:?}",
            welcome.compression
        ));
    }

    tracing::debug!(
        target: TRACE_TARGET,
        event = "handshake_end",
        role = "client",
        negotiated_version = welcome.version,
        negotiated_compression = ?welcome.compression
    );

    Ok(Negotiated {
        version: welcome.version,
        compression: welcome.compression,
    })
}

async fn server_handshake(
    stream: &mut BoxedStream,
    config: &ServerConfig,
) -> Result<(Negotiated, Option<String>)> {
    tracing::debug!(
        target: TRACE_TARGET,
        event = "handshake_start",
        role = "server",
        supported_versions = ?config.supported_versions,
        supported_compressions = ?config.supported_compressions
    );

    let packet = read_raw_packet(stream).await?;
    if packet.kind != PayloadKind::HandshakeHello || packet.request_id != 0 {
        return Err(anyhow!(
            "unexpected handshake packet: kind={:?} request_id={}",
            packet.kind,
            packet.request_id
        ));
    }
    let hello: ClientHello =
        bincode::deserialize(&packet.payload).context("decode client hello")?;

    let negotiated_version =
        negotiate_version(&config.supported_versions, &hello.supported_versions)
            .context("negotiate protocol version")?;
    let negotiated_compression = negotiate_compression(
        &config.supported_compressions,
        &hello.supported_compressions,
    )
    .context("negotiate compression")?;

    tracing::debug!(
        target: TRACE_TARGET,
        event = "handshake_end",
        role = "server",
        negotiated_version,
        negotiated_compression = ?negotiated_compression,
        peer_auth_present = hello.auth_token.is_some()
    );

    let welcome = ServerWelcome {
        version: negotiated_version,
        compression: negotiated_compression,
    };
    let payload = bincode::serialize(&welcome).context("encode server welcome")?;
    let payload_len: u32 = payload
        .len()
        .try_into()
        .map_err(|_| anyhow!("handshake payload too large"))?;
    write_raw_packet(
        stream,
        0,
        PayloadKind::HandshakeWelcome,
        false,
        payload_len,
        payload,
    )
    .await?;

    Ok((
        Negotiated {
            version: negotiated_version,
            compression: negotiated_compression,
        },
        hello.auth_token,
    ))
}

fn negotiate_version(server: &[u32], client: &[u32]) -> Result<u32> {
    server
        .iter()
        .copied()
        .filter(|version| client.contains(version))
        .max()
        .ok_or_else(|| anyhow!("no common protocol version"))
}

fn negotiate_compression(server: &[Compression], client: &[Compression]) -> Result<Compression> {
    let mut common: Vec<Compression> = server
        .iter()
        .copied()
        .filter(|algo| client.contains(algo))
        .collect();
    common.sort_by_key(|algo| match algo {
        Compression::None => 0,
        Compression::Zstd => 1,
    });
    common.pop().ok_or_else(|| anyhow!("no common compression"))
}

struct RawPacket {
    request_id: RequestId,
    kind: PayloadKind,
    compressed: bool,
    uncompressed_len: u32,
    payload: Vec<u8>,
}

async fn write_raw_packet(
    stream: &mut (impl AsyncWrite + Unpin),
    request_id: RequestId,
    kind: PayloadKind,
    compressed: bool,
    uncompressed_len: u32,
    payload: Vec<u8>,
) -> Result<()> {
    let wire_len: u32 = payload
        .len()
        .try_into()
        .map_err(|_| anyhow!("payload too large"))?;

    tracing::trace!(
        target: TRACE_TARGET,
        direction = "send",
        request_id,
        kind = ?kind,
        compressed,
        bytes = wire_len,
        uncompressed_bytes = uncompressed_len
    );

    stream
        .write_u64_le(request_id)
        .await
        .context("write request id")?;
    stream.write_u8(kind as u8).await.context("write kind")?;
    stream
        .write_u8(compressed as u8)
        .await
        .context("write compressed flag")?;
    stream
        .write_u32_le(wire_len)
        .await
        .context("write payload len")?;
    stream
        .write_u32_le(uncompressed_len)
        .await
        .context("write uncompressed len")?;
    stream.write_all(&payload).await.context("write payload")?;
    stream.flush().await.context("flush payload")?;
    Ok(())
}

async fn read_raw_packet(stream: &mut (impl AsyncRead + Unpin)) -> Result<RawPacket> {
    let request_id = stream.read_u64_le().await.context("read request id")?;
    let kind = PayloadKind::from_wire(stream.read_u8().await.context("read kind")?)?;
    let compressed = stream.read_u8().await.context("read compressed flag")? != 0;
    let wire_len = stream.read_u32_le().await.context("read payload len")?;
    let uncompressed_len = stream
        .read_u32_le()
        .await
        .context("read uncompressed len")?;
    let mut payload = vec![0u8; wire_len as usize];
    stream
        .read_exact(&mut payload)
        .await
        .context("read payload")?;

    tracing::trace!(
        target: TRACE_TARGET,
        direction = "recv",
        request_id,
        kind = ?kind,
        compressed,
        bytes = wire_len,
        uncompressed_bytes = uncompressed_len
    );

    Ok(RawPacket {
        request_id,
        kind,
        compressed,
        uncompressed_len,
        payload,
    })
}

fn maybe_compress(
    negotiated: &Negotiated,
    threshold: usize,
    payload: Vec<u8>,
) -> Result<(Vec<u8>, bool, u32)> {
    let uncompressed_len: u32 = payload
        .len()
        .try_into()
        .map_err(|_| anyhow!("payload too large"))?;

    if negotiated.compression == Compression::Zstd && payload.len() >= threshold {
        let compressed = zstd::bulk::compress(&payload, 3).context("zstd compress")?;
        if compressed.len() < payload.len() {
            return Ok((compressed, true, uncompressed_len));
        }
    }

    Ok((payload, false, uncompressed_len))
}

fn maybe_decompress(
    negotiated: &Negotiated,
    packet: RawPacket,
) -> Result<(PayloadKind, RequestId, Vec<u8>)> {
    let payload = if packet.compressed {
        match negotiated.compression {
            Compression::Zstd => {
                zstd::bulk::decompress(&packet.payload, packet.uncompressed_len as usize)
                    .context("zstd decompress")?
            }
            Compression::None => {
                return Err(anyhow!(
                    "received compressed packet but negotiated compression is None"
                ))
            }
        }
    } else {
        packet.payload
    };

    Ok((packet.kind, packet.request_id, payload))
}

async fn read_loop(
    mut reader: ReadHalf<BoxedStream>,
    inner: Arc<Inner>,
    incoming_tx: Option<mpsc::UnboundedSender<IncomingRequest>>,
) {
    loop {
        let packet = match read_raw_packet(&mut reader).await {
            Ok(packet) => packet,
            Err(err) => {
                tracing::debug!(
                    target: TRACE_TARGET,
                    event = "read_loop_end",
                    error = %err
                );
                let mut pending = inner.pending.lock().await;
                pending.clear();
                break;
            }
        };

        let request_id = packet.request_id;
        let (kind, request_id, payload) = match maybe_decompress(&inner.negotiated, packet) {
            Ok(decoded) => decoded,
            Err(err) => {
                tracing::debug!(
                    target: TRACE_TARGET,
                    event = "decode_error",
                    request_id,
                    error = %err
                );
                continue;
            }
        };

        match kind {
            PayloadKind::Response | PayloadKind::Error => {
                let tx = {
                    let mut pending = inner.pending.lock().await;
                    pending.remove(&request_id)
                };
                if let Some(tx) = tx {
                    let _ = tx.send(payload);
                } else {
                    tracing::trace!(
                        target: TRACE_TARGET,
                        event = "orphan_response",
                        request_id,
                        kind = ?kind,
                        bytes = payload.len()
                    );
                }
            }
            PayloadKind::Request => {
                if let Some(tx) = incoming_tx.as_ref() {
                    let _ = tx.send(IncomingRequest {
                        request_id,
                        payload,
                    });
                } else {
                    tracing::trace!(
                        target: TRACE_TARGET,
                        event = "unexpected_request",
                        request_id,
                        bytes = payload.len()
                    );
                }
            }
            PayloadKind::HandshakeHello | PayloadKind::HandshakeWelcome => {
                tracing::trace!(
                    target: TRACE_TARGET,
                    event = "unexpected_handshake_packet",
                    request_id,
                    kind = ?kind
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::sync::Mutex as StdMutex;

    use tracing_subscriber::fmt::MakeWriter;
    use tracing_subscriber::EnvFilter;

    #[derive(Clone, Default)]
    struct BufferWriter(Arc<StdMutex<Vec<u8>>>);

    struct BufferGuard(Arc<StdMutex<Vec<u8>>>);

    impl<'a> MakeWriter<'a> for BufferWriter {
        type Writer = BufferGuard;

        fn make_writer(&'a self) -> Self::Writer {
            BufferGuard(self.0.clone())
        }
    }

    impl io::Write for BufferGuard {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let mut guard = self.0.lock().unwrap();
            guard.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn client_server_roundtrip() -> Result<()> {
        let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);

        let server_task = tokio::spawn(async move {
            let mut server = Server::accept(server_stream, ServerConfig::default()).await?;
            let req = server
                .recv_request()
                .await
                .ok_or_else(|| anyhow!("missing request"))?;
            server.respond(req.request_id, b"pong".to_vec()).await?;
            Ok::<_, anyhow::Error>(())
        });

        let client = Client::connect(client_stream, ClientConfig::default()).await?;
        let resp = client.call(b"ping".to_vec()).await?;
        assert_eq!(resp, b"pong");

        server_task.await??;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handshake_does_not_log_auth_token() -> Result<()> {
        let buf = Arc::new(StdMutex::new(Vec::new()));
        let writer = BufferWriter(buf.clone());

        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::new("nova.remote_rpc=trace"))
            .with_writer(writer)
            .with_ansi(false)
            .finish();

        let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);

        let secret = "super-secret-token".to_string();
        let config = ClientConfig {
            auth_token: Some(secret.clone()),
            ..ClientConfig::default()
        };

        let fut = tracing::subscriber::with_default(subscriber, || async move {
            let server_task = tokio::spawn(async move {
                let _server = Server::accept(server_stream, ServerConfig::default()).await?;
                Ok::<_, anyhow::Error>(())
            });

            let _client = Client::connect(client_stream, config).await?;
            server_task.await??;
            Ok::<_, anyhow::Error>(())
        });

        fut.await?;

        let output = String::from_utf8(buf.lock().unwrap().clone()).context("decode output")?;
        assert!(
            !output.contains(&secret),
            "tracing output unexpectedly contained auth token"
        );

        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn compression_roundtrip_logs_compressed_packets() -> Result<()> {
        let buf = Arc::new(StdMutex::new(Vec::new()));
        let writer = BufferWriter(buf.clone());

        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::new("nova.remote_rpc=trace"))
            .with_writer(writer)
            .with_ansi(false)
            .finish();

        let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);

        let client_config = ClientConfig {
            compression_threshold: 1,
            ..ClientConfig::default()
        };
        let server_config = ServerConfig {
            compression_threshold: 1,
            ..ServerConfig::default()
        };

        let payload = vec![0u8; 4096];

        let fut = tracing::subscriber::with_default(subscriber, || {
            let payload = payload.clone();
            async move {
                let server_task = tokio::spawn(async move {
                    let mut server = Server::accept(server_stream, server_config).await?;
                    let req = server
                        .recv_request()
                        .await
                        .ok_or_else(|| anyhow!("missing request"))?;
                    assert_eq!(req.payload, payload);
                    server.respond(req.request_id, b"ok".to_vec()).await?;
                    Ok::<_, anyhow::Error>(())
                });

                let client = Client::connect(client_stream, client_config).await?;
                let resp = client.call(payload.clone()).await?;
                assert_eq!(resp, b"ok");

                server_task.await??;
                Ok::<_, anyhow::Error>(())
            }
        });

        fut.await?;

        let output = String::from_utf8(buf.lock().unwrap().clone()).context("decode output")?;
        assert!(
            output.contains("compressed=true"),
            "expected at least one compressed packet to be logged"
        );

        Ok(())
    }
}
