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
use tracing::Instrument;

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
    incoming: mpsc::UnboundedReceiver<IncomingRequest>,
}

pub struct Server {
    inner: Arc<Inner>,
    incoming: mpsc::UnboundedReceiver<IncomingRequest>,
    pub peer_shard_id: ShardId,
    pub peer_auth_present: bool,
}

/// Cloneable handle for issuing calls / sending responses on a connection.
///
/// This is intentionally separate from the request receiver so callers can dedicate a single task
/// to draining inbound requests while still issuing outbound calls from other tasks.
#[derive(Clone)]
pub struct Handle {
    inner: Arc<Inner>,
}

struct Inner {
    writer: Mutex<WriteHalf<BoxedStream>>,
    pending: Mutex<HashMap<RequestId, oneshot::Sender<RpcResult<Response>>>>,
    negotiated: Negotiated,
    compression_threshold: usize,
    worker_id: WorkerId,
    shard_id: ShardId,
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
        let (incoming_tx, incoming) = mpsc::unbounded_channel();
        let inner = Arc::new(Inner {
            writer: Mutex::new(writer),
            pending: Mutex::new(HashMap::new()),
            negotiated,
            compression_threshold: config.compression_threshold,
            worker_id: welcome.worker_id,
            shard_id: welcome.shard_id,
            // Worker-initiated request IDs are odd.
            next_request_id: AtomicU64::new(1),
            request_id_step: 2,
        });

        let inner_clone = inner.clone();
        tokio::spawn(async move { read_loop(reader, inner_clone, Some(incoming_tx)).await });

        Ok(Self {
            inner,
            welcome,
            incoming,
        })
    }

    pub fn welcome(&self) -> &RouterWelcome {
        &self.welcome
    }

    pub fn negotiated(&self) -> &Negotiated {
        &self.inner.negotiated
    }

    pub fn handle(&self) -> Handle {
        Handle {
            inner: self.inner.clone(),
        }
    }

    pub async fn recv_request(&mut self) -> Option<IncomingRequest> {
        self.incoming.recv().await
    }

    pub async fn respond(
        &self,
        request_id: RequestId,
        response: RpcResult<Response>,
    ) -> Result<()> {
        self.handle().respond(request_id, response).await
    }

    pub async fn respond_ok(&self, request_id: RequestId, value: Response) -> Result<()> {
        self.handle().respond_ok(request_id, value).await
    }

    pub async fn respond_err(&self, request_id: RequestId, error: RpcError) -> Result<()> {
        self.handle().respond_err(request_id, error).await
    }

    pub async fn call(&self, request: Request) -> Result<RpcResult<Response>> {
        self.handle().call(request).await
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
            worker_id: welcome.worker_id,
            shard_id: welcome.shard_id,
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

    pub fn handle(&self) -> Handle {
        Handle {
            inner: self.inner.clone(),
        }
    }

    pub async fn call(&self, request: Request) -> Result<RpcResult<Response>> {
        self.handle().call(request).await
    }

    pub async fn recv_request(&mut self) -> Option<IncomingRequest> {
        self.incoming.recv().await
    }

    pub async fn respond(
        &self,
        request_id: RequestId,
        response: RpcResult<Response>,
    ) -> Result<()> {
        self.handle().respond(request_id, response).await
    }

    pub async fn respond_ok(&self, request_id: RequestId, value: Response) -> Result<()> {
        self.handle().respond_ok(request_id, value).await
    }

    pub async fn respond_err(&self, request_id: RequestId, error: RpcError) -> Result<()> {
        self.handle().respond_err(request_id, error).await
    }
}

impl Handle {
    pub fn negotiated(&self) -> &Negotiated {
        &self.inner.negotiated
    }

    pub fn worker_id(&self) -> WorkerId {
        self.inner.worker_id
    }

    pub fn shard_id(&self) -> ShardId {
        self.inner.shard_id
    }

    pub async fn call(&self, request: Request) -> Result<RpcResult<Response>> {
        let request_id = self
            .inner
            .next_request_id
            .fetch_add(self.inner.request_id_step, Ordering::Relaxed);

        let request_type = request_type(&request);

        let start =
            tracing::enabled!(target: TRACE_TARGET, tracing::Level::DEBUG).then(Instant::now);

        let span = tracing::debug_span!(
            target: TRACE_TARGET,
            "call",
            worker_id = self.inner.worker_id,
            shard_id = self.inner.shard_id,
            request_id,
            request_type
        );
        let parent_span = span.clone();
        let inner = self.inner.clone();
        let response = async move {
            let (tx, rx) = oneshot::channel();
            {
                let mut pending = inner.pending.lock().await;
                pending.insert(request_id, tx);
            }

            if let Err(err) = inner
                .send_packet(request_id, RpcPayload::Request(request))
                .await
            {
                let mut pending = inner.pending.lock().await;
                pending.remove(&request_id);
                return Err(err);
            }

            rx.await
                .map_err(|_| anyhow!("connection closed while waiting for response"))
        }
        .instrument(span)
        .await?;

        if let Some(start) = start {
            let elapsed = start.elapsed();
            let status = rpc_result_status(&response);
            let error_code = rpc_result_error_code_str(&response);
            tracing::debug!(
                target: TRACE_TARGET,
                parent: &parent_span,
                event = "call_complete",
                worker_id = self.inner.worker_id,
                shard_id = self.inner.shard_id,
                request_id,
                request_type,
                status,
                error_code,
                latency_ms = elapsed.as_secs_f64() * 1000.0
            );
        }

        Ok(response)
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
        let payload_kind = payload_kind(&payload);
        let request_type = payload_request_type(&payload);
        let notification_type = payload_notification_type(&payload);
        let response_status = payload_response_status(&payload);
        let response_type = payload_response_type(&payload);
        let error_code = payload_error_code(&payload);

        let uncompressed = encode_payload(&payload)?;
        let (compression, wire_bytes) = maybe_compress(
            &self.negotiated.capabilities,
            self.compression_threshold,
            &uncompressed,
        )?;
        let chunked = self.negotiated.capabilities.supports_chunking
            && !packet_fits_in_single_frame(
                &wire_bytes,
                self.negotiated.capabilities.max_frame_len,
            );

        tracing::trace!(
            target: TRACE_TARGET,
            direction = "send",
            worker_id = self.worker_id,
            shard_id = self.shard_id,
            request_id,
            payload_kind,
            request_type,
            notification_type,
            response_status,
            response_type,
            error_code,
            compressed = compression != CompressionAlgo::None,
            chunked,
            bytes = wire_bytes.len(),
            uncompressed_bytes = uncompressed.len()
        );

        let mut writer = self.writer.lock().await;
        if chunked {
            write_packet_chunked(
                &mut *writer,
                self.negotiated.capabilities.max_frame_len,
                request_id,
                compression,
                wire_bytes,
            )
            .await
        } else {
            let frame = WireFrame::Packet {
                id: request_id,
                compression,
                data: wire_bytes,
            };
            write_wire_frame(
                &mut *writer,
                self.negotiated.capabilities.max_frame_len,
                &frame,
            )
            .await
        }
    }
}

fn packet_fits_in_single_frame(data: &[u8], max_frame_len: u32) -> bool {
    // We're encoding `WireFrame` as CBOR and then prefixing with a u32 length.
    // The CBOR overhead is small compared to typical payload sizes; we use a
    // conservative margin to decide whether to try the single-frame path.
    const WIRE_FRAME_OVERHEAD: usize = 256;
    data.len() + WIRE_FRAME_OVERHEAD <= max_frame_len as usize
}

async fn write_packet_chunked(
    stream: &mut (impl AsyncWrite + Unpin),
    max_frame_len: u32,
    request_id: RequestId,
    compression: CompressionAlgo,
    data: Vec<u8>,
) -> Result<()> {
    let empty = WireFrame::PacketChunk {
        id: request_id,
        compression,
        seq: 0,
        last: false,
        data: Vec::new(),
    };
    let empty_encoded = v3::encode_wire_frame(&empty).context("encode empty packet chunk")?;
    // Account for CBOR byte-string length header growth.
    let chunk_budget = max_frame_len
        .saturating_sub(empty_encoded.len().try_into().unwrap_or(u32::MAX))
        .saturating_sub(16) as usize;

    anyhow::ensure!(
        chunk_budget > 0,
        "max_frame_len too small for chunked packets: {max_frame_len}"
    );

    let chunks = data.chunks(chunk_budget);
    let total_chunks = chunks.len();
    for (idx, chunk) in chunks.enumerate() {
        let seq: u32 = idx.try_into().unwrap_or(u32::MAX);
        let last = idx + 1 == total_chunks;
        let frame = WireFrame::PacketChunk {
            id: request_id,
            compression,
            seq,
            last,
            data: chunk.to_vec(),
        };
        write_wire_frame(stream, max_frame_len, &frame).await?;
    }

    Ok(())
}

async fn client_handshake(
    stream: &mut BoxedStream,
    config: &ClientConfig,
) -> Result<RouterWelcome> {
    let start = tracing::enabled!(target: TRACE_TARGET, tracing::Level::DEBUG).then(Instant::now);

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
    .await
    .map_err(|err| {
        let latency_ms = start.map(|start| start.elapsed().as_secs_f64() * 1000.0);
        tracing::debug!(
            target: TRACE_TARGET,
            event = "handshake_end",
            role = "worker",
            status = "error",
            latency_ms,
            error = %err
        );
        err
    })?;

    let frame = match read_wire_frame(stream, config.pre_handshake_max_frame_len).await {
        Ok(frame) => frame,
        Err(err) => {
            if let Some(start) = start {
                tracing::debug!(
                    target: TRACE_TARGET,
                    event = "handshake_end",
                    role = "worker",
                    status = "error",
                    latency_ms = start.elapsed().as_secs_f64() * 1000.0,
                    error = %err
                );
            } else {
                tracing::debug!(
                    target: TRACE_TARGET,
                    event = "handshake_end",
                    role = "worker",
                    status = "error",
                    error = %err
                );
            }
            return Err(err);
        }
    };
    match frame {
        WireFrame::Welcome(welcome) => {
            let latency_ms = start.map(|start| start.elapsed().as_secs_f64() * 1000.0);
            tracing::debug!(
                target: TRACE_TARGET,
                event = "handshake_end",
                role = "worker",
                status = "ok",
                worker_id = welcome.worker_id,
                shard_id = welcome.shard_id,
                revision = welcome.revision,
                negotiated_version = ?welcome.chosen_version,
                negotiated_capabilities = ?welcome.chosen_capabilities,
                latency_ms
            );
            Ok(welcome)
        }
        WireFrame::Reject(reject) => {
            let latency_ms = start.map(|start| start.elapsed().as_secs_f64() * 1000.0);
            tracing::debug!(
                target: TRACE_TARGET,
                event = "handshake_end",
                role = "worker",
                status = "rejected",
                reject_code = ?reject.code,
                latency_ms,
                message = %reject.message
            );

            Err(anyhow!(
                "handshake rejected (code={:?}): {}",
                reject.code,
                reject.message
            ))
        }
        other => {
            let frame_type = wire_frame_type(&other);
            let latency_ms = start.map(|start| start.elapsed().as_secs_f64() * 1000.0);
            tracing::debug!(
                target: TRACE_TARGET,
                event = "handshake_end",
                role = "worker",
                status = "error",
                latency_ms,
                frame_type
            );
            Err(anyhow!("unexpected handshake frame: {frame_type}"))
        }
    }
}

async fn server_handshake(
    stream: &mut BoxedStream,
    config: &ServerConfig,
) -> Result<(RouterWelcome, ShardId, bool)> {
    let start = tracing::enabled!(target: TRACE_TARGET, tracing::Level::DEBUG).then(Instant::now);

    tracing::debug!(
        target: TRACE_TARGET,
        event = "handshake_start",
        role = "router",
        supported_versions = ?config.supported_versions,
        capabilities = ?config.capabilities,
        auth_required = config.expected_auth_token.is_some()
    );

    let frame = match read_wire_frame(stream, config.pre_handshake_max_frame_len).await {
        Ok(frame) => frame,
        Err(err) => {
            if let Some(start) = start {
                tracing::debug!(
                    target: TRACE_TARGET,
                    event = "handshake_end",
                    role = "router",
                    status = "error",
                    latency_ms = start.elapsed().as_secs_f64() * 1000.0,
                    error = %err
                );
            } else {
                tracing::debug!(
                    target: TRACE_TARGET,
                    event = "handshake_end",
                    role = "router",
                    status = "error",
                    error = %err
                );
            }
            return Err(err);
        }
    };
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
            let latency_ms = start.map(|start| start.elapsed().as_secs_f64() * 1000.0);
            tracing::debug!(
                target: TRACE_TARGET,
                event = "handshake_end",
                role = "router",
                status = "rejected",
                reject_code = ?RejectCode::InvalidRequest,
                latency_ms,
                message = "expected hello"
            );
            return Err(anyhow!(
                "invalid handshake: expected hello, got {frame_type}"
            ));
        }
    };

    tracing::debug!(
        target: TRACE_TARGET,
        event = "handshake_hello",
        role = "router",
        shard_id = hello.shard_id,
        peer_supported_versions = ?hello.supported_versions,
        peer_capabilities = ?hello.capabilities,
        peer_auth_present = hello.auth_token.is_some(),
        cached_index_present = hello.cached_index_info.is_some(),
        worker_build_present = hello.worker_build.is_some()
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
            let latency_ms = start.map(|start| start.elapsed().as_secs_f64() * 1000.0);
            tracing::debug!(
                target: TRACE_TARGET,
                event = "handshake_end",
                role = "router",
                status = "rejected",
                reject_code = ?RejectCode::Unauthorized,
                latency_ms
            );
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
        let latency_ms = start.map(|start| start.elapsed().as_secs_f64() * 1000.0);
        tracing::debug!(
            target: TRACE_TARGET,
            event = "handshake_end",
            role = "router",
            status = "rejected",
            reject_code = ?RejectCode::UnsupportedVersion,
            latency_ms
        );
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
                let latency_ms = start.map(|start| start.elapsed().as_secs_f64() * 1000.0);
                tracing::debug!(
                    target: TRACE_TARGET,
                    event = "handshake_end",
                    role = "router",
                    status = "rejected",
                    reject_code = ?RejectCode::InvalidRequest,
                    latency_ms
                );
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

    let latency_ms = start.map(|start| start.elapsed().as_secs_f64() * 1000.0);
    tracing::debug!(
        target: TRACE_TARGET,
        event = "handshake_end",
        role = "router",
        status = "ok",
        worker_id = welcome.worker_id,
        shard_id = welcome.shard_id,
        revision = welcome.revision,
        negotiated_version = ?welcome.chosen_version,
        negotiated_capabilities = ?welcome.chosen_capabilities,
        peer_auth_present = hello.auth_token.is_some(),
        latency_ms
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
    write_wire_frame_bytes(stream, max_frame_len, &payload).await
}

async fn write_wire_frame_bytes(
    stream: &mut (impl AsyncWrite + Unpin),
    max_frame_len: u32,
    payload: &[u8],
) -> Result<()> {
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

    // Use fallible reservation so allocation failure surfaces as an error rather than aborting the
    // process.
    let len_usize = len as usize;
    let mut buf = Vec::new();
    buf.try_reserve_exact(len_usize)
        .with_context(|| format!("allocate frame buffer ({len} bytes)"))?;
    buf.resize(len_usize, 0);
    stream
        .read_exact(&mut buf)
        .await
        .context("read frame payload")?;
    v3::decode_wire_frame(&buf).context("decode wire frame")
}

fn encode_payload(payload: &RpcPayload) -> Result<Vec<u8>> {
    v3::encode_rpc_payload(payload).context("encode rpc payload")
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
            let mut out = Vec::new();
            out.try_reserve_exact(data.len())
                .context("allocate packet buffer")?;
            out.extend_from_slice(data);
            Ok(out)
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
        out.try_reserve(n)
            .context("allocate decompression buffer")?;
        out.extend_from_slice(&buf[..n]);
    }
    Ok(out)
}

async fn read_loop(
    mut reader: ReadHalf<BoxedStream>,
    inner: Arc<Inner>,
    incoming_tx: Option<mpsc::UnboundedSender<IncomingRequest>>,
) {
    struct ChunkState {
        compression: CompressionAlgo,
        next_seq: u32,
        data: Vec<u8>,
    }

    let mut chunked_packets: HashMap<RequestId, ChunkState> = HashMap::new();
    loop {
        let frame =
            match read_wire_frame(&mut reader, inner.negotiated.capabilities.max_frame_len).await {
                Ok(frame) => frame,
                Err(err) => {
                    tracing::debug!(
                        target: TRACE_TARGET,
                        event = "read_loop_end",
                        worker_id = inner.worker_id,
                        shard_id = inner.shard_id,
                        error = %err
                    );
                    let mut pending = inner.pending.lock().await;
                    pending.clear();
                    break;
                }
            };

        let (request_id, compression, data, chunked) = match frame {
            WireFrame::Packet {
                id,
                compression,
                data,
            } => (id, compression, data, false),
            WireFrame::PacketChunk {
                id,
                compression,
                seq,
                last,
                data,
            } => {
                if !inner.negotiated.capabilities.supports_chunking {
                    tracing::debug!(
                        target: TRACE_TARGET,
                        event = "unsupported_chunk",
                        worker_id = inner.worker_id,
                        shard_id = inner.shard_id,
                        request_id = id
                    );
                    continue;
                }

                let max_packet_len = inner.negotiated.capabilities.max_packet_len as usize;
                let entry = chunked_packets.entry(id).or_insert_with(|| ChunkState {
                    compression,
                    next_seq: 0,
                    data: Vec::new(),
                });

                if entry.compression != compression || entry.next_seq != seq {
                    tracing::debug!(
                        target: TRACE_TARGET,
                        event = "chunk_out_of_order",
                        worker_id = inner.worker_id,
                        shard_id = inner.shard_id,
                        request_id = id,
                        expected_seq = entry.next_seq,
                        got_seq = seq
                    );
                    chunked_packets.remove(&id);
                    let mut pending = inner.pending.lock().await;
                    pending.clear();
                    break;
                }
                entry.next_seq = entry.next_seq.saturating_add(1);

                if entry.data.len() + data.len() > max_packet_len {
                    tracing::debug!(
                        target: TRACE_TARGET,
                        event = "chunk_too_large",
                        worker_id = inner.worker_id,
                        shard_id = inner.shard_id,
                        request_id = id,
                        current = entry.data.len(),
                        incoming = data.len(),
                        max_packet_len
                    );
                    chunked_packets.remove(&id);
                    let mut pending = inner.pending.lock().await;
                    pending.clear();
                    break;
                }

                entry.data.extend_from_slice(&data);

                if !last {
                    continue;
                }

                let entry = chunked_packets.remove(&id).expect("entry exists");
                (id, entry.compression, entry.data, true)
            }
            other => {
                tracing::trace!(
                    target: TRACE_TARGET,
                    event = "unexpected_frame",
                    worker_id = inner.worker_id,
                    shard_id = inner.shard_id,
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
                        worker_id = inner.worker_id,
                        shard_id = inner.shard_id,
                        request_id,
                        error = %err
                    );
                    let mut pending = inner.pending.lock().await;
                    pending.clear();
                    break;
                }
            };

        let payload: RpcPayload = match v3::decode_rpc_payload(&decoded_bytes) {
            Ok(payload) => payload,
            Err(err) => {
                tracing::debug!(
                    target: TRACE_TARGET,
                    event = "decode_error",
                    worker_id = inner.worker_id,
                    shard_id = inner.shard_id,
                    request_id,
                    error = %err
                );
                let mut pending = inner.pending.lock().await;
                pending.clear();
                break;
            }
        };

        tracing::trace!(
            target: TRACE_TARGET,
            direction = "recv",
            worker_id = inner.worker_id,
            shard_id = inner.shard_id,
            request_id,
            payload_kind = payload_kind(&payload),
            request_type = payload_request_type(&payload),
            notification_type = payload_notification_type(&payload),
            response_status = payload_response_status(&payload),
            response_type = payload_response_type(&payload),
            error_code = payload_error_code(&payload),
            compressed,
            chunked,
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
                        worker_id = inner.worker_id,
                        shard_id = inner.shard_id,
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
                        worker_id = inner.worker_id,
                        shard_id = inner.shard_id,
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

fn payload_request_type(payload: &RpcPayload) -> &'static str {
    match payload {
        RpcPayload::Request(request) => request_type(request),
        _ => "",
    }
}

fn payload_notification_type(payload: &RpcPayload) -> &'static str {
    match payload {
        RpcPayload::Notification(notification) => notification_type(notification),
        _ => "",
    }
}

fn payload_response_status(payload: &RpcPayload) -> &'static str {
    match payload {
        RpcPayload::Response(result) => rpc_result_status(result),
        _ => "",
    }
}

fn payload_response_type(payload: &RpcPayload) -> &'static str {
    match payload {
        RpcPayload::Response(RpcResult::Ok { value }) => response_type(value),
        _ => "",
    }
}

fn payload_error_code(payload: &RpcPayload) -> &'static str {
    match payload {
        RpcPayload::Response(RpcResult::Err { error }) => rpc_error_code_str(error.code),
        _ => "",
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

fn notification_type(notification: &nova_remote_proto::v3::Notification) -> &'static str {
    match notification {
        nova_remote_proto::v3::Notification::CachedIndex(_) => "cached_index",
        nova_remote_proto::v3::Notification::Unknown => "unknown",
    }
}

fn response_type(response: &Response) -> &'static str {
    match response {
        Response::Ack => "ack",
        Response::ShardIndex(_) => "shard_index",
        Response::WorkerStats(_) => "worker_stats",
        Response::Shutdown => "shutdown",
        Response::Unknown => "unknown",
    }
}

fn rpc_result_status(result: &RpcResult<Response>) -> &'static str {
    match result {
        RpcResult::Ok { .. } => "ok",
        RpcResult::Err { .. } => "err",
        RpcResult::Unknown => "unknown",
    }
}

fn rpc_result_error_code_str(result: &RpcResult<Response>) -> &'static str {
    match result {
        RpcResult::Err { error } => rpc_error_code_str(error.code),
        _ => "",
    }
}

fn rpc_error_code_str(code: RpcErrorCode) -> &'static str {
    match code {
        RpcErrorCode::InvalidRequest => "invalid_request",
        RpcErrorCode::Unauthorized => "unauthorized",
        RpcErrorCode::UnsupportedVersion => "unsupported_version",
        RpcErrorCode::TooLarge => "too_large",
        RpcErrorCode::Cancelled => "cancelled",
        RpcErrorCode::Internal => "internal",
        RpcErrorCode::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::sync::Mutex as StdMutex;

    use proptest::prelude::*;
    use proptest::test_runner::TestRunner;
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
            assert_eq!(
                req.request_id % 2,
                1,
                "worker-initiated request IDs are odd"
            );
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
    async fn router_initiated_roundtrip() -> Result<()> {
        let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);

        let server_task = tokio::spawn(async move {
            let server = Server::accept(server_stream, ServerConfig::default()).await?;
            let resp = server.call(Request::GetWorkerStats).await?;
            match resp {
                RpcResult::Ok { value } => assert!(matches!(value, Response::Ack)),
                other => return Err(anyhow!("unexpected response: {other:?}")),
            }
            Ok::<_, anyhow::Error>(())
        });

        let mut client = Client::connect(client_stream, ClientConfig::default()).await?;
        let req = client
            .recv_request()
            .await
            .ok_or_else(|| anyhow!("missing request"))?;
        assert_eq!(
            req.request_id % 2,
            0,
            "router-initiated request IDs are even"
        );
        assert!(matches!(req.request, Request::GetWorkerStats));
        client.respond_ok(req.request_id, Response::Ack).await?;

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
        let _guard = tracing::subscriber::set_default(subscriber);

        let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);

        let secret = "super-secret-token".to_string();

        let mut client_cfg = ClientConfig::default();
        client_cfg.hello.auth_token = Some(secret.clone());

        let mut server_cfg = ServerConfig::default();
        server_cfg.expected_auth_token = Some(secret.clone());

        let server_task = tokio::spawn(async move {
            let _server = Server::accept(server_stream, server_cfg).await?;
            Ok::<_, anyhow::Error>(())
        });

        let _client = Client::connect(client_stream, client_cfg).await?;
        server_task.await??;

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
        let _guard = tracing::subscriber::set_default(subscriber);

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

        let output = String::from_utf8(buf.lock().unwrap().clone()).context("decode output")?;
        assert!(
            output.contains("compressed=true"),
            "expected at least one compressed packet to be logged; output was:\n{output}"
        );

        Ok(())
    }

    #[test]
    fn read_wire_frame_rejects_oversize_len_prefix_without_allocating() {
        // A regression test for the length-prefixed framing: `read_wire_frame` must reject
        // lengths larger than `max_frame_len` *before* allocating the buffer.
        //
        // If the check happens after allocation, this test would try to allocate ~4GiB and likely
        // OOM the process.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build tokio runtime");

        rt.block_on(async {
            let max_frame_len = 1024u32;
            let len = u32::MAX;

            let mut bytes = Vec::new();
            bytes.extend_from_slice(&len.to_le_bytes());

            let (mut tx, mut rx) = tokio::io::duplex(bytes.len());
            tx.write_all(&bytes).await.expect("write prefix");
            drop(tx);

            let err = read_wire_frame(&mut rx, max_frame_len)
                .await
                .expect_err("expected oversize frame error");
            assert!(
                err.to_string().contains("frame too large"),
                "unexpected error: {err:?}"
            );
        });
    }

    #[test]
    fn read_wire_frame_never_panics_on_random_bytes() {
        const MAX_FUZZ_INPUT_LEN: usize = 64 * 1024;
        const MAX_FRAME_LEN: u32 = 64 * 1024;

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build tokio runtime");

        let mut runner = TestRunner::new(ProptestConfig {
            cases: 64,
            ..ProptestConfig::default()
        });
        runner
            .run(
                &proptest::collection::vec(any::<u8>(), 0..=MAX_FUZZ_INPUT_LEN),
                |bytes| {
                    rt.block_on(async {
                        let cap = bytes.len().max(1);
                        let (mut tx, mut rx) = tokio::io::duplex(cap);
                        tx.write_all(&bytes)
                            .await
                            .expect("write fuzz input to duplex");
                        drop(tx);

                        let _ = read_wire_frame(&mut rx, MAX_FRAME_LEN).await;
                    });
                    Ok(())
                },
            )
            .unwrap();
    }

    #[test]
    fn maybe_decompress_never_panics_on_random_bytes() {
        const MAX_FUZZ_INPUT_LEN: usize = 16 * 1024;

        let compression = prop_oneof![
            Just(CompressionAlgo::None),
            Just(CompressionAlgo::Zstd),
            Just(CompressionAlgo::Unknown),
        ];

        let mut runner = TestRunner::new(ProptestConfig {
            cases: 64,
            ..ProptestConfig::default()
        });
        runner
            .run(
                &(
                    proptest::collection::vec(any::<u8>(), 0..=MAX_FUZZ_INPUT_LEN),
                    0u32..=8192u32,
                    compression,
                ),
                |(data, max_packet_len, compression)| {
                    let mut caps = Capabilities::default();
                    caps.max_packet_len = max_packet_len;

                    let _ = maybe_decompress(&caps, compression, &data);
                    Ok(())
                },
            )
            .unwrap();
    }

    #[test]
    fn maybe_decompress_zstd_respects_max_packet_len() -> Result<()> {
        let uncompressed = vec![b'x'; 1025];
        let compressed = zstd::bulk::compress(&uncompressed, 3).context("compress zstd")?;

        let mut caps = Capabilities::default();
        caps.max_packet_len = 1024;

        let err = maybe_decompress(&caps, CompressionAlgo::Zstd, &compressed)
            .expect_err("expected error");
        assert!(
            err.to_string().contains("decompressed payload too large"),
            "unexpected error: {err:?}"
        );

        Ok(())
    }
}
