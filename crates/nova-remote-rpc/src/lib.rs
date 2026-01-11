use std::collections::HashMap;
use std::fmt;
use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use nova_remote_proto::v3::{
    self, Capabilities, CompressionAlgo, HandshakeReject, ProtocolVersion, RejectCode, Request,
    Response, RouterWelcome, RpcError, RpcErrorCode, RpcPayload, RpcResult, SupportedVersions,
    WireFrame, WorkerHello,
};
#[cfg(test)]
use nova_remote_proto::FileText;
use nova_remote_proto::{Revision, ShardId, WorkerId};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::sync::{mpsc, oneshot, Mutex};

/// The `tracing` target used by this crate.
pub const TRACE_TARGET: &str = "nova.remote_rpc";

/// Maximum allowed frame size before the handshake completes.
///
/// The v3 protocol requires a small, local (non-negotiated) guard here to avoid allocating
/// attacker-controlled lengths before we've validated the peer's capabilities.
pub const DEFAULT_PRE_HANDSHAKE_MAX_FRAME_LEN: u32 = 1024 * 1024; // 1 MiB

pub type RequestId = u64;

#[derive(Debug, Clone)]
pub struct Negotiated {
    pub version: ProtocolVersion,
    pub capabilities: Capabilities,
}

#[derive(Clone)]
pub struct ClientConfig {
    /// Worker-side hello payload (includes supported versions/capabilities).
    ///
    /// NOTE: `auth_token` is treated as a bearer secret and must never be logged.
    pub hello: WorkerHello,
    pub pre_handshake_max_frame_len: u32,
    /// Compress payloads larger than this threshold (if zstd is negotiated).
    pub compression_threshold: usize,
}

impl fmt::Debug for ClientConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientConfig")
            .field("shard_id", &self.hello.shard_id)
            .field("auth_present", &self.hello.auth_token.is_some())
            .field("supported_versions", &self.hello.supported_versions)
            .field("capabilities", &self.hello.capabilities)
            .field("cached_index_info", &self.hello.cached_index_info)
            .field("worker_build", &self.hello.worker_build)
            .field(
                "pre_handshake_max_frame_len",
                &self.pre_handshake_max_frame_len,
            )
            .field("compression_threshold", &self.compression_threshold)
            .finish()
    }
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            hello: WorkerHello {
                shard_id: 0,
                auth_token: None,
                supported_versions: SupportedVersions {
                    min: ProtocolVersion::CURRENT,
                    max: ProtocolVersion::CURRENT,
                },
                capabilities: Capabilities {
                    supported_compression: vec![CompressionAlgo::Zstd, CompressionAlgo::None],
                    ..Capabilities::default()
                },
                cached_index_info: None,
                worker_build: None,
            },
            pre_handshake_max_frame_len: DEFAULT_PRE_HANDSHAKE_MAX_FRAME_LEN,
            compression_threshold: 1024,
        }
    }
}

#[derive(Clone)]
pub struct ServerConfig {
    pub supported_versions: SupportedVersions,
    pub capabilities: Capabilities,
    pub pre_handshake_max_frame_len: u32,
    pub compression_threshold: usize,
    pub worker_id: WorkerId,
    pub revision: Revision,
    /// Optional bearer token expected from the worker.
    ///
    /// NOTE: This is secret material and must never be logged.
    pub expected_auth_token: Option<String>,
}

impl fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerConfig")
            .field("supported_versions", &self.supported_versions)
            .field("capabilities", &self.capabilities)
            .field(
                "pre_handshake_max_frame_len",
                &self.pre_handshake_max_frame_len,
            )
            .field("compression_threshold", &self.compression_threshold)
            .field("worker_id", &self.worker_id)
            .field("revision", &self.revision)
            .field("auth_required", &self.expected_auth_token.is_some())
            .finish()
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            supported_versions: SupportedVersions {
                min: ProtocolVersion::CURRENT,
                max: ProtocolVersion::CURRENT,
            },
            capabilities: Capabilities {
                supported_compression: vec![CompressionAlgo::Zstd, CompressionAlgo::None],
                ..Capabilities::default()
            },
            pre_handshake_max_frame_len: DEFAULT_PRE_HANDSHAKE_MAX_FRAME_LEN,
            compression_threshold: 1024,
            worker_id: 1,
            revision: 0,
            expected_auth_token: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct IncomingRequest {
    pub request_id: RequestId,
    pub request: Request,
}

pub struct Client {
    inner: Arc<Inner>,
    welcome: RouterWelcome,
}

pub struct Server {
    inner: Arc<Inner>,
    incoming: mpsc::UnboundedReceiver<IncomingRequest>,
    pub peer_shard_id: ShardId,
    pub peer_auth_present: bool,
}

struct Inner {
    writer: Mutex<WriteHalf<BoxedStream>>,
    pending: Mutex<HashMap<RequestId, oneshot::Sender<RpcResult<Response>>>>,
    negotiated: Negotiated,
    compression_threshold: usize,
    next_request_id: AtomicU64,
    request_id_step: u64,
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
        let welcome = client_handshake(&mut stream, &config).await?;

        let negotiated = Negotiated {
            version: welcome.chosen_version,
            capabilities: welcome.chosen_capabilities.clone(),
        };

        let (reader, writer) = tokio::io::split(stream);
        let inner = Arc::new(Inner {
            writer: Mutex::new(writer),
            pending: Mutex::new(HashMap::new()),
            negotiated,
            compression_threshold: config.compression_threshold,
            // Worker-initiated request IDs are odd.
            next_request_id: AtomicU64::new(1),
            request_id_step: 2,
        });

        let inner_clone = inner.clone();
        tokio::spawn(async move { read_loop(reader, inner_clone, None).await });

        Ok(Self { inner, welcome })
    }

    pub fn welcome(&self) -> &RouterWelcome {
        &self.welcome
    }

    pub fn negotiated(&self) -> &Negotiated {
        &self.inner.negotiated
    }

    pub async fn call(&self, request: Request) -> Result<RpcResult<Response>> {
        let request_id = self
            .inner
            .next_request_id
            .fetch_add(self.inner.request_id_step, Ordering::Relaxed);

        let request_type = request_type(&request);

        let start = tracing::enabled!(target: TRACE_TARGET, level: tracing::Level::DEBUG)
            .then(Instant::now);

        let span = tracing::debug_span!(
            target: TRACE_TARGET,
            "call",
            request_id,
            request_type
        );
        let _guard = span.enter();

        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.inner.pending.lock().await;
            pending.insert(request_id, tx);
        }

        if let Err(err) = self
            .inner
            .send_packet(request_id, RpcPayload::Request(request))
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
            let status = rpc_result_status(&response);
            let error_code = rpc_result_error_code(&response);
            tracing::debug!(
                target: TRACE_TARGET,
                event = "call_complete",
                request_id,
                request_type,
                status,
                error_code,
                latency_ms = elapsed.as_secs_f64() * 1000.0
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

        let (welcome, peer_shard_id, peer_auth_present) =
            server_handshake(&mut stream, &config).await?;

        let negotiated = Negotiated {
            version: welcome.chosen_version,
            capabilities: welcome.chosen_capabilities.clone(),
        };

        let (reader, writer) = tokio::io::split(stream);
        let (incoming_tx, incoming) = mpsc::unbounded_channel();

        let inner = Arc::new(Inner {
            writer: Mutex::new(writer),
            pending: Mutex::new(HashMap::new()),
            negotiated,
            compression_threshold: config.compression_threshold,
            // Router-initiated request IDs are even.
            next_request_id: AtomicU64::new(2),
            request_id_step: 2,
        });

        let inner_clone = inner.clone();
        tokio::spawn(async move { read_loop(reader, inner_clone, Some(incoming_tx)).await });

        Ok(Self {
            inner,
            incoming,
            peer_shard_id,
            peer_auth_present,
        })
    }

    pub fn negotiated(&self) -> &Negotiated {
        &self.inner.negotiated
    }

    pub async fn recv_request(&mut self) -> Option<IncomingRequest> {
        self.incoming.recv().await
    }

    pub async fn respond(
        &self,
        request_id: RequestId,
        response: RpcResult<Response>,
    ) -> Result<()> {
        self.inner
            .send_packet(request_id, RpcPayload::Response(response))
            .await
    }

    pub async fn respond_ok(&self, request_id: RequestId, value: Response) -> Result<()> {
        self.respond(request_id, RpcResult::Ok { value }).await
    }

    pub async fn respond_err(&self, request_id: RequestId, error: RpcError) -> Result<()> {
        self.respond(request_id, RpcResult::Err { error }).await
    }
}

impl Inner {
    async fn send_packet(&self, request_id: RequestId, payload: RpcPayload) -> Result<()> {
        let (uncompressed, payload_kind) = encode_payload(&payload)?;
        let (compression, wire_bytes) = maybe_compress(
            &self.negotiated.capabilities,
            self.compression_threshold,
            &uncompressed,
        )?;

        tracing::trace!(
            target: TRACE_TARGET,
            direction = "send",
            request_id,
            payload_kind,
            compressed = compression != CompressionAlgo::None,
            bytes = wire_bytes.len(),
            uncompressed_bytes = uncompressed.len()
        );

        let frame = WireFrame::Packet {
            id: request_id,
            compression,
            data: wire_bytes,
        };

        let mut writer = self.writer.lock().await;
        write_wire_frame(
            &mut *writer,
            self.negotiated.capabilities.max_frame_len,
            &frame,
        )
        .await
    }
}

async fn client_handshake(
    stream: &mut BoxedStream,
    config: &ClientConfig,
) -> Result<RouterWelcome> {
    tracing::debug!(
        target: TRACE_TARGET,
        event = "handshake_start",
        role = "worker",
        shard_id = config.hello.shard_id,
        supported_versions = ?config.hello.supported_versions,
        capabilities = ?config.hello.capabilities,
        auth_present = config.hello.auth_token.is_some()
    );

    write_wire_frame(
        stream,
        config.pre_handshake_max_frame_len,
        &WireFrame::Hello(config.hello.clone()),
    )
    .await?;

    let frame = read_wire_frame(stream, config.pre_handshake_max_frame_len).await?;
    match frame {
        WireFrame::Welcome(welcome) => {
            tracing::debug!(
                target: TRACE_TARGET,
                event = "handshake_end",
                role = "worker",
                worker_id = welcome.worker_id,
                shard_id = welcome.shard_id,
                revision = welcome.revision,
                negotiated_version = ?welcome.chosen_version,
                negotiated_capabilities = ?welcome.chosen_capabilities
            );
            Ok(welcome)
        }
        WireFrame::Reject(reject) => Err(anyhow!(
            "handshake rejected (code={:?}): {}",
            reject.code,
            reject.message
        )),
        other => Err(anyhow!(
            "unexpected handshake frame: {}",
            wire_frame_type(&other)
        )),
    }
}

async fn server_handshake(
    stream: &mut BoxedStream,
    config: &ServerConfig,
) -> Result<(RouterWelcome, ShardId, bool)> {
    let frame = read_wire_frame(stream, config.pre_handshake_max_frame_len).await?;
    let hello = match frame {
        WireFrame::Hello(hello) => hello,
        other => {
            let frame_type = wire_frame_type(&other);
            let reject = HandshakeReject {
                code: RejectCode::InvalidRequest,
                message: format!("expected hello, got {frame_type}"),
            };
            let reject_frame = WireFrame::Reject(reject);
            let _ =
                write_wire_frame(stream, config.pre_handshake_max_frame_len, &reject_frame).await;
            return Err(anyhow!(
                "invalid handshake: expected hello, got {frame_type}"
            ));
        }
    };

    tracing::debug!(
        target: TRACE_TARGET,
        event = "handshake_start",
        role = "router",
        shard_id = hello.shard_id,
        supported_versions = ?config.supported_versions,
        capabilities = ?config.capabilities,
        peer_supported_versions = ?hello.supported_versions,
        peer_capabilities = ?hello.capabilities,
        peer_auth_present = hello.auth_token.is_some(),
        auth_required = config.expected_auth_token.is_some()
    );

    if let Some(expected) = config.expected_auth_token.as_deref() {
        let provided = hello.auth_token.as_deref().unwrap_or("");
        if provided != expected {
            let reject = HandshakeReject {
                code: RejectCode::Unauthorized,
                message: "invalid auth token".to_string(),
            };
            write_wire_frame(
                stream,
                config.pre_handshake_max_frame_len,
                &WireFrame::Reject(reject.clone()),
            )
            .await?;
            return Err(anyhow!("handshake rejected: unauthorized"));
        }
    }

    let Some(chosen_version) = hello
        .supported_versions
        .choose_common(&config.supported_versions)
    else {
        let reject = HandshakeReject {
            code: RejectCode::UnsupportedVersion,
            message: "no common protocol version".to_string(),
        };
        let _ = write_wire_frame(
            stream,
            config.pre_handshake_max_frame_len,
            &WireFrame::Reject(reject),
        )
        .await;
        return Err(anyhow!("handshake rejected: unsupported version"));
    };

    let chosen_capabilities =
        match negotiate_capabilities(&config.capabilities, &hello.capabilities) {
            Ok(caps) => caps,
            Err(err) => {
                let reject = HandshakeReject {
                    code: RejectCode::InvalidRequest,
                    message: err.to_string(),
                };
                let _ = write_wire_frame(
                    stream,
                    config.pre_handshake_max_frame_len,
                    &WireFrame::Reject(reject),
                )
                .await;
                return Err(err).context("handshake rejected: incompatible capabilities");
            }
        };

    let welcome = RouterWelcome {
        worker_id: config.worker_id,
        shard_id: hello.shard_id,
        revision: config.revision,
        chosen_version,
        chosen_capabilities,
    };

    write_wire_frame(
        stream,
        config.pre_handshake_max_frame_len,
        &WireFrame::Welcome(welcome.clone()),
    )
    .await?;

    tracing::debug!(
        target: TRACE_TARGET,
        event = "handshake_end",
        role = "router",
        worker_id = welcome.worker_id,
        shard_id = welcome.shard_id,
        revision = welcome.revision,
        negotiated_version = ?welcome.chosen_version,
        negotiated_capabilities = ?welcome.chosen_capabilities,
        peer_auth_present = hello.auth_token.is_some()
    );

    Ok((welcome, hello.shard_id, hello.auth_token.is_some()))
}

fn negotiate_capabilities(router: &Capabilities, worker: &Capabilities) -> Result<Capabilities> {
    let max_frame_len = router.max_frame_len.min(worker.max_frame_len);
    let max_packet_len = router.max_packet_len.min(worker.max_packet_len);
    let supports_cancel = router.supports_cancel && worker.supports_cancel;
    let supports_chunking = router.supports_chunking && worker.supports_chunking;

    let supported_compression: Vec<CompressionAlgo> = router
        .supported_compression
        .iter()
        .copied()
        .filter(|algo| {
            *algo != CompressionAlgo::Unknown
                && worker.supported_compression.contains(algo)
                && *algo != CompressionAlgo::Unknown
        })
        .collect();

    if supported_compression.is_empty() {
        return Err(anyhow!("no common compression algorithm"));
    }

    Ok(Capabilities {
        max_frame_len,
        max_packet_len,
        supported_compression,
        supports_cancel,
        supports_chunking,
    })
}

async fn write_wire_frame(
    stream: &mut (impl AsyncWrite + Unpin),
    max_frame_len: u32,
    frame: &WireFrame,
) -> Result<()> {
    let payload = v3::encode_wire_frame(frame).context("encode wire frame")?;
    let len: u32 = payload
        .len()
        .try_into()
        .map_err(|_| anyhow!("frame too large"))?;
    if len > max_frame_len {
        return Err(anyhow!(
            "frame too large: {len} bytes (limit {max_frame_len} bytes)"
        ));
    }

    stream.write_u32_le(len).await.context("write frame len")?;
    stream
        .write_all(&payload)
        .await
        .context("write frame payload")?;
    stream.flush().await.context("flush frame")?;
    Ok(())
}

async fn read_wire_frame(
    stream: &mut (impl AsyncRead + Unpin),
    max_frame_len: u32,
) -> Result<WireFrame> {
    let len = stream.read_u32_le().await.context("read frame len")?;
    if len > max_frame_len {
        return Err(anyhow!(
            "frame too large: {len} bytes (limit {max_frame_len} bytes)"
        ));
    }

    let mut buf = vec![0u8; len as usize];
    stream
        .read_exact(&mut buf)
        .await
        .context("read frame payload")?;
    v3::decode_wire_frame(&buf).context("decode wire frame")
}

fn encode_payload(payload: &RpcPayload) -> Result<(Vec<u8>, &'static str)> {
    let kind = payload_kind(payload);
    let bytes = v3::encode_rpc_payload(payload).context("encode rpc payload")?;
    Ok((bytes, kind))
}

fn maybe_compress(
    negotiated: &Capabilities,
    threshold: usize,
    uncompressed: &[u8],
) -> Result<(CompressionAlgo, Vec<u8>)> {
    if uncompressed.len() > negotiated.max_packet_len as usize {
        return Err(anyhow!(
            "payload too large: {} bytes (limit {} bytes)",
            uncompressed.len(),
            negotiated.max_packet_len
        ));
    }

    let allow_zstd = negotiated
        .supported_compression
        .iter()
        .any(|algo| *algo == CompressionAlgo::Zstd);

    if allow_zstd && uncompressed.len() >= threshold {
        let compressed = zstd::bulk::compress(uncompressed, 3).context("zstd compress")?;
        if compressed.len() < uncompressed.len() {
            return Ok((CompressionAlgo::Zstd, compressed));
        }
    }

    Ok((CompressionAlgo::None, uncompressed.to_vec()))
}

fn maybe_decompress(
    negotiated: &Capabilities,
    compression: CompressionAlgo,
    data: &[u8],
) -> Result<Vec<u8>> {
    let max_packet_len = negotiated.max_packet_len as usize;
    match compression {
        CompressionAlgo::None => {
            if data.len() > max_packet_len {
                return Err(anyhow!(
                    "payload too large: {} bytes (limit {} bytes)",
                    data.len(),
                    max_packet_len
                ));
            }
            Ok(data.to_vec())
        }
        CompressionAlgo::Zstd => decompress_zstd_with_limit(data, max_packet_len),
        CompressionAlgo::Unknown => Err(anyhow!("unsupported compression algorithm: unknown")),
    }
}

fn decompress_zstd_with_limit(data: &[u8], limit: usize) -> Result<Vec<u8>> {
    let mut decoder = zstd::stream::read::Decoder::new(data).context("create zstd decoder")?;
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = decoder.read(&mut buf).context("read zstd stream")?;
        if n == 0 {
            break;
        }
        if out.len() + n > limit {
            return Err(anyhow!(
                "decompressed payload too large: {} bytes (limit {} bytes)",
                out.len() + n,
                limit
            ));
        }
        out.extend_from_slice(&buf[..n]);
    }
    Ok(out)
}

async fn read_loop(
    mut reader: ReadHalf<BoxedStream>,
    inner: Arc<Inner>,
    incoming_tx: Option<mpsc::UnboundedSender<IncomingRequest>>,
) {
    loop {
        let frame =
            match read_wire_frame(&mut reader, inner.negotiated.capabilities.max_frame_len).await {
                Ok(frame) => frame,
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

        let (request_id, compression, data) = match frame {
            WireFrame::Packet {
                id,
                compression,
                data,
            } => (id, compression, data),
            WireFrame::PacketChunk { id, .. } => {
                tracing::debug!(
                    target: TRACE_TARGET,
                    event = "unsupported_chunk",
                    request_id = id
                );
                continue;
            }
            other => {
                tracing::trace!(
                    target: TRACE_TARGET,
                    event = "unexpected_frame",
                    frame_type = wire_frame_type(&other)
                );
                continue;
            }
        };

        let compressed = compression != CompressionAlgo::None;
        let decoded_bytes =
            match maybe_decompress(&inner.negotiated.capabilities, compression, &data) {
                Ok(bytes) => bytes,
                Err(err) => {
                    tracing::debug!(
                        target: TRACE_TARGET,
                        event = "decompress_error",
                        request_id,
                        error = %err
                    );
                    continue;
                }
            };

        let payload: RpcPayload = match v3::decode_rpc_payload(&decoded_bytes) {
            Ok(payload) => payload,
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

        tracing::trace!(
            target: TRACE_TARGET,
            direction = "recv",
            request_id,
            payload_kind = payload_kind(&payload),
            compressed,
            bytes = data.len(),
            uncompressed_bytes = decoded_bytes.len()
        );

        match payload {
            RpcPayload::Response(response) => {
                let tx = {
                    let mut pending = inner.pending.lock().await;
                    pending.remove(&request_id)
                };
                if let Some(tx) = tx {
                    let _ = tx.send(response);
                } else {
                    tracing::trace!(
                        target: TRACE_TARGET,
                        event = "orphan_response",
                        request_id
                    );
                }
            }
            RpcPayload::Request(request) => {
                if let Some(tx) = incoming_tx.as_ref() {
                    let _ = tx.send(IncomingRequest {
                        request_id,
                        request,
                    });
                } else {
                    tracing::trace!(
                        target: TRACE_TARGET,
                        event = "unexpected_request",
                        request_id
                    );
                }
            }
            RpcPayload::Notification(_) | RpcPayload::Cancel | RpcPayload::Unknown => {
                // Not routed yet; callers can enable `trace` to see the packet metadata.
            }
        }
    }
}

fn payload_kind(payload: &RpcPayload) -> &'static str {
    match payload {
        RpcPayload::Request(_) => "request",
        RpcPayload::Response(_) => "response",
        RpcPayload::Notification(_) => "notification",
        RpcPayload::Cancel => "cancel",
        RpcPayload::Unknown => "unknown",
    }
}

fn wire_frame_type(frame: &WireFrame) -> &'static str {
    match frame {
        WireFrame::Hello(_) => "hello",
        WireFrame::Welcome(_) => "welcome",
        WireFrame::Reject(_) => "reject",
        WireFrame::Packet { .. } => "packet",
        WireFrame::PacketChunk { .. } => "packet_chunk",
        WireFrame::Unknown => "unknown",
    }
}

fn request_type(request: &Request) -> &'static str {
    match request {
        Request::LoadFiles { .. } => "load_files",
        Request::IndexShard { .. } => "index_shard",
        Request::UpdateFile { .. } => "update_file",
        Request::GetWorkerStats => "get_worker_stats",
        Request::Shutdown => "shutdown",
        Request::Unknown => "unknown",
    }
}

fn rpc_result_status(result: &RpcResult<Response>) -> &'static str {
    match result {
        RpcResult::Ok { .. } => "ok",
        RpcResult::Err { .. } => "err",
        RpcResult::Unknown => "unknown",
    }
}

fn rpc_result_error_code(result: &RpcResult<Response>) -> Option<RpcErrorCode> {
    match result {
        RpcResult::Err { error } => Some(error.code),
        _ => None,
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
            assert!(matches!(req.request, Request::GetWorkerStats));
            server.respond_ok(req.request_id, Response::Ack).await?;
            Ok::<_, anyhow::Error>(())
        });

        let client = Client::connect(client_stream, ClientConfig::default()).await?;
        let resp = client.call(Request::GetWorkerStats).await?;
        match resp {
            RpcResult::Ok { value } => assert!(matches!(value, Response::Ack)),
            other => return Err(anyhow!("unexpected response: {other:?}")),
        }

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

        let mut client_cfg = ClientConfig::default();
        client_cfg.hello.auth_token = Some(secret.clone());

        let mut server_cfg = ServerConfig::default();
        server_cfg.expected_auth_token = Some(secret.clone());

        let fut = tracing::subscriber::with_default(subscriber, || async move {
            let server_task = tokio::spawn(async move {
                let _server = Server::accept(server_stream, server_cfg).await?;
                Ok::<_, anyhow::Error>(())
            });

            let _client = Client::connect(client_stream, client_cfg).await?;
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

        let mut client_cfg = ClientConfig::default();
        client_cfg.compression_threshold = 1;

        let mut server_cfg = ServerConfig::default();
        server_cfg.compression_threshold = 1;

        let large_text = "x".repeat(16 * 1024);
        let request = Request::LoadFiles {
            revision: 1,
            files: vec![FileText {
                path: "a.java".into(),
                text: large_text,
            }],
        };

        let fut = tracing::subscriber::with_default(subscriber, || async move {
            let server_task = tokio::spawn(async move {
                let mut server = Server::accept(server_stream, server_cfg).await?;
                let req = server
                    .recv_request()
                    .await
                    .ok_or_else(|| anyhow!("missing request"))?;
                assert!(matches!(req.request, Request::LoadFiles { .. }));
                server.respond_ok(req.request_id, Response::Ack).await?;
                Ok::<_, anyhow::Error>(())
            });

            let client = Client::connect(client_stream, client_cfg).await?;
            let resp = client.call(request).await?;
            match resp {
                RpcResult::Ok { value } => assert!(matches!(value, Response::Ack)),
                other => return Err(anyhow!("unexpected response: {other:?}")),
            }

            server_task.await??;
            Ok::<_, anyhow::Error>(())
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
