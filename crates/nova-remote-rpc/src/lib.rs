//! Tokio-based transport/runtime for Nova's v3 remote RPC protocol.
//!
//! This crate implements:
//! - u32 length-prefixed framing with strict size checks before allocation
//! - request id allocation with router/worker parity (router = even, worker = odd)
//! - multiplexed concurrent in-flight calls
//! - packet chunking (`WireFrame::PacketChunk`) with interleaving reassembly
//! - optional `zstd` compression (feature: `zstd`)
//! - structured remote errors (`nova_remote_proto::v3::RpcError`) and cancellation packets

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use bytes::Bytes;
use nova_remote_proto::v3::{
    self, Capabilities, CompressionAlgo, HandshakeReject, Notification, ProtocolVersion, RejectCode,
    Request, Response, RouterWelcome, RpcError as ProtoRpcError, RpcErrorCode, RpcPayload,
    RpcResult, SupportedVersions, WireFrame, WorkerHello,
};
use nova_remote_proto::{Revision, WorkerId};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, watch, Mutex};

pub type RequestId = u64;

/// Which side of the connection we are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcRole {
    Router,
    Worker,
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum RpcTransportError {
    #[error("I/O error: {message}")]
    Io { message: String },

    #[error("allocation failed: {message}")]
    AllocationFailed { message: String },

    #[error("frame too large: {len} > {max}")]
    FrameTooLarge { len: u32, max: u32 },

    #[error("packet too large: {len} > {max}")]
    PacketTooLarge { len: usize, max: usize },

    #[error("decode error: {message}")]
    DecodeError { message: String },

    #[error("encode error: {message}")]
    EncodeError { message: String },

    #[error("handshake failed: {message}")]
    HandshakeFailed { message: String },

    #[error("unsupported compression algorithm: {algo:?}")]
    UnsupportedCompression { algo: CompressionAlgo },

    #[error("protocol violation: {message}")]
    ProtocolViolation { message: String },

    #[error("connection closed")]
    ConnectionClosed,
}

impl From<std::io::Error> for RpcTransportError {
    fn from(err: std::io::Error) -> Self {
        RpcTransportError::Io {
            message: err.to_string(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    #[error(transparent)]
    Transport(#[from] RpcTransportError),

    #[error("remote error: {0:?}")]
    Remote(ProtoRpcError),

    #[error("request cancelled")]
    Canceled,

    #[error("unexpected response payload")]
    UnexpectedResponse,
}

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

type RequestHandler =
    Arc<dyn Fn(RequestContext, Request) -> BoxFuture<Result<Response, ProtoRpcError>> + Send + Sync>;
type NotificationHandler =
    Arc<dyn Fn(Notification) -> BoxFuture<()> + Send + Sync + 'static>;
type CancelHandler = Arc<dyn Fn(RequestId) + Send + Sync + 'static>;

/// A lightweight cancellation token for incoming requests.
#[derive(Debug, Clone)]
pub struct CancellationToken {
    rx: watch::Receiver<bool>,
}

impl CancellationToken {
    pub fn is_cancelled(&self) -> bool {
        *self.rx.borrow()
    }

    pub async fn cancelled(&mut self) {
        while self.rx.changed().await.is_ok() {
            if *self.rx.borrow() {
                return;
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct RequestContext {
    request_id: RequestId,
    cancel: CancellationToken,
}

impl RequestContext {
    pub fn request_id(&self) -> RequestId {
        self.request_id
    }

    pub fn cancellation(&self) -> CancellationToken {
        self.cancel.clone()
    }
}

/// Worker-side (client) transport configuration.
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    pub hello: WorkerHello,
    pub pre_handshake_max_frame_len: u32,
    pub compression_threshold: usize,
}

impl WorkerConfig {
    pub fn new(hello: WorkerHello) -> Self {
        Self {
            hello,
            pre_handshake_max_frame_len: DEFAULT_PRE_HANDSHAKE_MAX_FRAME_LEN,
            compression_threshold: DEFAULT_COMPRESSION_THRESHOLD,
        }
    }
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self::new(default_worker_hello())
    }
}

/// Router-side (server) transport configuration.
#[derive(Debug, Clone)]
pub struct RouterConfig {
    pub supported_versions: SupportedVersions,
    pub capabilities: Capabilities,
    pub pre_handshake_max_frame_len: u32,
    pub compression_threshold: usize,
    pub worker_id: WorkerId,
    pub revision: Revision,
    pub expected_auth_token: Option<String>,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            supported_versions: SupportedVersions {
                min: ProtocolVersion::CURRENT,
                max: ProtocolVersion::CURRENT,
            },
            capabilities: default_capabilities(),
            pre_handshake_max_frame_len: DEFAULT_PRE_HANDSHAKE_MAX_FRAME_LEN,
            compression_threshold: DEFAULT_COMPRESSION_THRESHOLD,
            worker_id: 1,
            revision: 0,
            expected_auth_token: None,
        }
    }
}

#[derive(Clone)]
pub struct RpcConnection {
    inner: Arc<Inner>,
    welcome: RouterWelcome,
}

impl RpcConnection {
    pub async fn handshake_as_router<S>(
        stream: S,
        expected_auth_token: Option<&str>,
    ) -> Result<(Self, RouterWelcome), RpcTransportError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let mut cfg = RouterConfig::default();
        cfg.expected_auth_token = expected_auth_token.map(|s| s.to_string());
        Self::handshake_as_router_with_config(stream, cfg).await
    }

    pub async fn handshake_as_router_with_config<S>(
        mut stream: S,
        mut cfg: RouterConfig,
    ) -> Result<(Self, RouterWelcome), RpcTransportError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        sanitize_capabilities(&mut cfg.capabilities);

        let hello = match read_wire_frame(&mut stream, cfg.pre_handshake_max_frame_len).await? {
            WireFrame::Hello(hello) => hello,
            other => {
                return Err(RpcTransportError::HandshakeFailed {
                    message: format!("expected hello frame, got {}", wire_frame_type(&other)),
                })
            }
        };

        if let Some(expected) = cfg.expected_auth_token.as_deref() {
            if hello.auth_token.as_deref() != Some(expected) {
                let reject = HandshakeReject {
                    code: RejectCode::Unauthorized,
                    message: "authentication failed".into(),
                };
                let _ = write_wire_frame(
                    &mut stream,
                    cfg.pre_handshake_max_frame_len,
                    &WireFrame::Reject(reject),
                )
                .await;
                return Err(RpcTransportError::HandshakeFailed {
                    message: "authentication failed".into(),
                });
            }
        }

        let Some(chosen_version) = cfg
            .supported_versions
            .choose_common(&hello.supported_versions)
        else {
            let reject = HandshakeReject {
                code: RejectCode::UnsupportedVersion,
                message: "unsupported protocol version".into(),
            };
            let _ = write_wire_frame(
                &mut stream,
                cfg.pre_handshake_max_frame_len,
                &WireFrame::Reject(reject),
            )
            .await;
            return Err(RpcTransportError::HandshakeFailed {
                message: "unsupported protocol version".into(),
            });
        };

        let chosen_capabilities = match negotiate_capabilities(&cfg.capabilities, &hello.capabilities)
        {
            Ok(caps) => caps,
            Err(err) => {
                let reject = HandshakeReject {
                    code: RejectCode::InvalidRequest,
                    message: err.to_string(),
                };
                let _ = write_wire_frame(
                    &mut stream,
                    cfg.pre_handshake_max_frame_len,
                    &WireFrame::Reject(reject),
                )
                .await;
                return Err(err);
            }
        };

        let welcome = RouterWelcome {
            worker_id: cfg.worker_id,
            shard_id: hello.shard_id,
            revision: cfg.revision,
            chosen_version,
            chosen_capabilities: chosen_capabilities.clone(),
        };

        write_wire_frame(
            &mut stream,
            cfg.pre_handshake_max_frame_len,
            &WireFrame::Welcome(welcome.clone()),
        )
        .await?;

        let conn = RpcConnection::start(
            stream,
            RpcRole::Router,
            chosen_version,
            chosen_capabilities,
            cfg.compression_threshold,
            welcome.clone(),
        );
        Ok((conn, welcome))
    }

    pub async fn handshake_as_worker<S>(
        stream: S,
        hello: WorkerHello,
    ) -> Result<(Self, RouterWelcome), RpcTransportError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        Self::handshake_as_worker_with_config(stream, WorkerConfig::new(hello)).await
    }

    pub async fn handshake_as_worker_with_config<S>(
        mut stream: S,
        mut cfg: WorkerConfig,
    ) -> Result<(Self, RouterWelcome), RpcTransportError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        sanitize_capabilities(&mut cfg.hello.capabilities);

        write_wire_frame(
            &mut stream,
            cfg.pre_handshake_max_frame_len,
            &WireFrame::Hello(cfg.hello.clone()),
        )
        .await?;

        let frame = read_wire_frame(&mut stream, cfg.pre_handshake_max_frame_len).await?;
        match frame {
            WireFrame::Welcome(welcome) => {
                let conn = RpcConnection::start(
                    stream,
                    RpcRole::Worker,
                    welcome.chosen_version,
                    welcome.chosen_capabilities.clone(),
                    cfg.compression_threshold,
                    welcome.clone(),
                );
                Ok((conn, welcome))
            }
            WireFrame::Reject(reject) => Err(RpcTransportError::HandshakeFailed {
                message: format!("handshake rejected (code={:?}): {}", reject.code, reject.message),
            }),
            other => Err(RpcTransportError::HandshakeFailed {
                message: format!(
                    "unexpected handshake frame: {}",
                    wire_frame_type(&other)
                ),
            }),
        }
    }

    pub fn welcome(&self) -> &RouterWelcome {
        &self.welcome
    }

    pub fn negotiated_capabilities(&self) -> &Capabilities {
        &self.inner.capabilities
    }

    pub fn negotiated_version(&self) -> ProtocolVersion {
        self.inner.version
    }

    pub fn role(&self) -> RpcRole {
        self.inner.role
    }

    pub fn set_request_handler<H, Fut>(&self, handler: H)
    where
        H: Fn(RequestContext, Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Response, ProtoRpcError>> + Send + 'static,
    {
        let handler: RequestHandler = Arc::new(move |ctx, req| Box::pin(handler(ctx, req)));
        *self.inner.request_handler.write().unwrap() = Some(handler);
    }

    pub fn set_notification_handler<H, Fut>(&self, handler: H)
    where
        H: Fn(Notification) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let handler: NotificationHandler = Arc::new(move |n| Box::pin(handler(n)));
        *self.inner.notification_handler.write().unwrap() = Some(handler);
    }

    pub fn set_cancel_handler<H>(&self, handler: H)
    where
        H: Fn(RequestId) + Send + Sync + 'static,
    {
        *self.inner.cancel_handler.write().unwrap() = Some(Arc::new(handler));
    }

    pub async fn call(&self, request: Request) -> Result<Response, RpcError> {
        if let Some(err) = self.inner.is_closed().await {
            return Err(RpcError::Transport(err));
        }

        let request_id = self.inner.alloc_id();
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.inner.pending.lock().await;
            pending.insert(request_id, tx);
        }

        if let Err(err) = send_rpc_payload(&self.inner, request_id, RpcPayload::Request(request)).await
        {
            let mut pending = self.inner.pending.lock().await;
            pending.remove(&request_id);
            return Err(RpcError::Transport(err));
        }

        match rx.await {
            Ok(res) => res,
            Err(_) => Err(RpcError::Transport(RpcTransportError::ConnectionClosed)),
        }
    }

    pub async fn notify(&self, notification: Notification) -> Result<(), RpcTransportError> {
        if let Some(err) = self.inner.is_closed().await {
            return Err(err);
        }
        let id = self.inner.alloc_id();
        send_rpc_payload(&self.inner, id, RpcPayload::Notification(notification)).await
    }

    pub async fn cancel(&self, request_id: RequestId) -> Result<(), RpcTransportError> {
        if let Some(err) = self.inner.is_closed().await {
            return Err(err);
        }
        send_rpc_payload(&self.inner, request_id, RpcPayload::Cancel).await
    }

    pub async fn shutdown(&self) -> Result<(), RpcTransportError> {
        self.inner.close(RpcTransportError::ConnectionClosed).await;
        Ok(())
    }

    fn start<S>(
        stream: S,
        role: RpcRole,
        version: ProtocolVersion,
        capabilities: Capabilities,
        compression_threshold: usize,
        welcome: RouterWelcome,
    ) -> Self
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (tx, rx) = mpsc::channel::<Bytes>(256);

        // Request-id parity rule:
        // - Router-initiated request IDs are even.
        // - Worker-initiated request IDs are odd.
        let next_request_id = match role {
            RpcRole::Router => 2,
            RpcRole::Worker => 1,
        };

        let inner = Arc::new(Inner {
            role,
            version,
            capabilities: capabilities.clone(),
            compression_threshold,
            next_request_id: AtomicU64::new(next_request_id),
            request_id_step: 2,
            tx,
            shutdown_tx,
            closed: tokio::sync::Mutex::new(None),
            pending: Mutex::new(HashMap::new()),
            incoming_cancels: Mutex::new(HashMap::new()),
            request_handler: RwLock::new(None),
            notification_handler: RwLock::new(None),
            cancel_handler: RwLock::new(None),
            max_inflight_chunked_packets: MAX_INFLIGHT_CHUNKED_PACKETS,
            max_reassembly_bytes: MAX_REASSEMBLY_BYTES,
        });

        let (read_half, write_half) = tokio::io::split(stream);
        tokio::spawn(read_loop(read_half, inner.clone(), shutdown_rx.clone()));
        tokio::spawn(write_loop(write_half, inner.clone(), shutdown_rx, rx));

        Self { inner, welcome }
    }
}

struct Inner {
    role: RpcRole,
    version: ProtocolVersion,
    capabilities: Capabilities,
    compression_threshold: usize,
    next_request_id: AtomicU64,
    request_id_step: u64,
    tx: mpsc::Sender<Bytes>,
    shutdown_tx: watch::Sender<bool>,
    closed: tokio::sync::Mutex<Option<RpcTransportError>>,

    pending: Mutex<HashMap<RequestId, oneshot::Sender<Result<Response, RpcError>>>>,
    incoming_cancels: Mutex<HashMap<RequestId, watch::Sender<bool>>>,

    request_handler: RwLock<Option<RequestHandler>>,
    notification_handler: RwLock<Option<NotificationHandler>>,
    cancel_handler: RwLock<Option<CancelHandler>>,

    max_inflight_chunked_packets: usize,
    max_reassembly_bytes: usize,
}

impl Inner {
    fn alloc_id(&self) -> RequestId {
        loop {
            let current = self.next_request_id.load(Ordering::Relaxed);
            let mut next = current.wrapping_add(self.request_id_step);
            if next == 0 {
                next = next.wrapping_add(self.request_id_step);
            }
            if self
                .next_request_id
                .compare_exchange(current, next, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return current;
            }
        }
    }

    async fn close(&self, err: RpcTransportError) {
        {
            let mut guard = self.closed.lock().await;
            if guard.is_some() {
                return;
            }
            *guard = Some(err.clone());
        }

        let _ = self.shutdown_tx.send(true);

        let mut pending = self.pending.lock().await;
        for (_, tx) in pending.drain() {
            let _ = tx.send(Err(RpcError::Transport(err.clone())));
        }
    }

    async fn is_closed(&self) -> Option<RpcTransportError> {
        self.closed.lock().await.clone()
    }
}

/// Maximum allowed frame size before the handshake completes.
///
/// The v3 protocol requires a small, local (non-negotiated) guard here to avoid allocating
/// attacker-controlled lengths before we've validated the peer's capabilities.
pub const DEFAULT_PRE_HANDSHAKE_MAX_FRAME_LEN: u32 = 1024 * 1024; // 1 MiB
const DEFAULT_COMPRESSION_THRESHOLD: usize = 1024;

const MAX_INFLIGHT_CHUNKED_PACKETS: usize = 32;
const MAX_REASSEMBLY_BYTES: usize = 256 * 1024 * 1024;

/// Conservative headroom for CBOR overhead when chunking.
const CHUNK_OVERHEAD_GUESS: usize = 256;

fn default_worker_hello() -> WorkerHello {
    WorkerHello {
        shard_id: 0,
        auth_token: None,
        supported_versions: SupportedVersions {
            min: ProtocolVersion::CURRENT,
            max: ProtocolVersion::CURRENT,
        },
        capabilities: default_capabilities(),
        cached_index_info: None,
        worker_build: None,
    }
}

fn default_capabilities() -> Capabilities {
    let mut caps = Capabilities::default();
    caps.supported_compression = local_supported_compression();
    caps.supports_cancel = true;
    caps.supports_chunking = true;
    caps
}

fn local_supported_compression() -> Vec<CompressionAlgo> {
    #[cfg(feature = "zstd")]
    {
        return vec![CompressionAlgo::Zstd, CompressionAlgo::None];
    }
    #[cfg(not(feature = "zstd"))]
    {
        return vec![CompressionAlgo::None];
    }
}

fn sanitize_capabilities(caps: &mut Capabilities) {
    let local = local_supported_compression();
    caps.supported_compression
        .retain(|algo| *algo != CompressionAlgo::Unknown && local.contains(algo));
    if !caps.supported_compression.contains(&CompressionAlgo::None) {
        caps.supported_compression.push(CompressionAlgo::None);
    }
    if caps.supported_compression.is_empty() {
        caps.supported_compression.push(CompressionAlgo::None);
    }
}

fn negotiate_capabilities(router: &Capabilities, worker: &Capabilities) -> Result<Capabilities, RpcTransportError> {
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
        return Err(RpcTransportError::HandshakeFailed {
            message: "no common compression algorithm".into(),
        });
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
) -> Result<(), RpcTransportError> {
    let payload = v3::encode_wire_frame(frame).map_err(|err| RpcTransportError::EncodeError {
        message: err.to_string(),
    })?;
    let len: u32 = payload
        .len()
        .try_into()
        .map_err(|_| RpcTransportError::FrameTooLarge {
            len: u32::MAX,
            max: max_frame_len,
        })?;
    if len > max_frame_len {
        return Err(RpcTransportError::FrameTooLarge {
            len,
            max: max_frame_len,
        });
    }

    stream.write_u32_le(len).await?;
    stream.write_all(&payload).await?;
    stream.flush().await?;
    Ok(())
}

async fn read_wire_frame(
    stream: &mut (impl AsyncRead + Unpin),
    max_frame_len: u32,
) -> Result<WireFrame, RpcTransportError> {
    let len = stream.read_u32_le().await?;
    if len > max_frame_len {
        return Err(RpcTransportError::FrameTooLarge {
            len,
            max: max_frame_len,
        });
    }

    // Reserve fallibly so allocation failure surfaces as an error instead of aborting the process.
    let len_usize = len as usize;
    let mut buf = Vec::new();
    buf.try_reserve_exact(len_usize).map_err(|err| {
        RpcTransportError::AllocationFailed {
            message: format!("allocate frame buffer ({len} bytes): {err}"),
        }
    })?;
    buf.resize(len_usize, 0);
    stream.read_exact(&mut buf).await?;
    v3::decode_wire_frame(&buf).map_err(|err| RpcTransportError::DecodeError {
        message: err.to_string(),
    })
}

async fn write_loop<W: AsyncWrite + Unpin + Send + 'static>(
    mut w: W,
    inner: Arc<Inner>,
    mut shutdown_rx: watch::Receiver<bool>,
    mut rx: mpsc::Receiver<Bytes>,
) {
    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
            bytes = rx.recv() => {
                let Some(bytes) = bytes else { break; };
                if bytes.len() > inner.capabilities.max_frame_len as usize {
                    inner.close(RpcTransportError::FrameTooLarge {
                        len: bytes.len().min(u32::MAX as usize) as u32,
                        max: inner.capabilities.max_frame_len,
                    }).await;
                    break;
                }

                let len: u32 = match bytes.len().try_into() {
                    Ok(len) => len,
                    Err(_) => {
                        inner.close(RpcTransportError::FrameTooLarge {
                            len: u32::MAX,
                            max: inner.capabilities.max_frame_len,
                        }).await;
                        break;
                    }
                };

                if let Err(err) = w.write_u32_le(len).await {
                    inner.close(RpcTransportError::from(err)).await;
                    break;
                }
                if let Err(err) = w.write_all(&bytes).await {
                    inner.close(RpcTransportError::from(err)).await;
                    break;
                }
                if let Err(err) = w.flush().await {
                    inner.close(RpcTransportError::from(err)).await;
                    break;
                }
            }
        }
    }

    let _ = w.shutdown().await;
}

async fn read_loop<R: AsyncRead + Unpin + Send + 'static>(
    mut r: R,
    inner: Arc<Inner>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    #[derive(Debug)]
    struct Reassembly {
        compression: CompressionAlgo,
        next_seq: u32,
        buf: Vec<u8>,
    }

    let mut in_flight: HashMap<RequestId, Reassembly> = HashMap::new();
    let mut total_bytes: usize = 0;

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
            res = read_wire_frame(&mut r, inner.capabilities.max_frame_len) => {
                let frame = match res {
                    Ok(frame) => frame,
                    Err(err) => {
                        inner.close(err).await;
                        break;
                    }
                };

                match frame {
                    WireFrame::Packet { id, compression, data } => {
                        if let Err(err) = process_packet(&inner, id, compression, data).await {
                            inner.close(err).await;
                            break;
                        }
                    }
                    WireFrame::PacketChunk { id, compression, seq, last, data } => {
                        if !inner.capabilities.supports_chunking {
                            inner.close(RpcTransportError::ProtocolViolation {
                                message: "received chunked packet but chunking not negotiated".into(),
                            }).await;
                            break;
                        }

                        if !in_flight.contains_key(&id) {
                            if in_flight.len() >= inner.max_inflight_chunked_packets {
                                inner.close(RpcTransportError::ProtocolViolation {
                                    message: "too many in-flight chunked packets".into(),
                                }).await;
                                break;
                            }
                            in_flight.insert(id, Reassembly { compression, next_seq: 0, buf: Vec::new() });
                        }

                        let Some(entry) = in_flight.get_mut(&id) else {
                            inner.close(RpcTransportError::ProtocolViolation {
                                message: "missing chunk reassembly entry".into(),
                            }).await;
                            break;
                        };

                        if entry.compression != compression {
                            inner.close(RpcTransportError::ProtocolViolation {
                                message: "chunk compression changed mid-stream".into(),
                            }).await;
                            break;
                        }

                        if entry.next_seq != seq {
                            inner.close(RpcTransportError::ProtocolViolation {
                                message: format!("chunk seq mismatch for id {id}: expected {}, got {seq}", entry.next_seq),
                            }).await;
                            break;
                        }
                        entry.next_seq = entry.next_seq.wrapping_add(1);

                        if total_bytes.saturating_add(data.len()) > inner.max_reassembly_bytes {
                            inner.close(RpcTransportError::ProtocolViolation {
                                message: "reassembly buffer limit exceeded".into(),
                            }).await;
                            break;
                        }
                        if entry.buf.len().saturating_add(data.len()) > inner.capabilities.max_packet_len as usize {
                            inner.close(RpcTransportError::PacketTooLarge {
                                len: entry.buf.len().saturating_add(data.len()),
                                max: inner.capabilities.max_packet_len as usize,
                            }).await;
                            break;
                        }

                        entry.buf.extend_from_slice(&data);
                        total_bytes += data.len();

                        if last {
                            let entry = in_flight.remove(&id).expect("entry present");
                            total_bytes = total_bytes.saturating_sub(entry.buf.len());
                            if let Err(err) = process_packet(&inner, id, entry.compression, entry.buf).await {
                                inner.close(err).await;
                                break;
                            }
                        }
                    }
                    WireFrame::Hello(_) | WireFrame::Welcome(_) | WireFrame::Reject(_) => {
                        inner.close(RpcTransportError::ProtocolViolation {
                            message: "unexpected handshake frame after handshake".into(),
                        }).await;
                        break;
                    }
                    WireFrame::Unknown => {
                        // Ignore forward-compatible frames.
                    }
                }
            }
        }
    }
}

async fn process_packet(
    inner: &Arc<Inner>,
    request_id: RequestId,
    compression: CompressionAlgo,
    data: Vec<u8>,
) -> Result<(), RpcTransportError> {
    if data.len() > inner.capabilities.max_packet_len as usize {
        return Err(RpcTransportError::PacketTooLarge {
            len: data.len(),
            max: inner.capabilities.max_packet_len as usize,
        });
    }

    let decoded = maybe_decompress(&inner.capabilities, compression, &data)?;
    let payload = v3::decode_rpc_payload(&decoded).map_err(|err| RpcTransportError::DecodeError {
        message: err.to_string(),
    })?;
    handle_payload(inner.clone(), request_id, payload).await
}

async fn handle_payload(
    inner: Arc<Inner>,
    request_id: RequestId,
    payload: RpcPayload,
) -> Result<(), RpcTransportError> {
    match payload {
        RpcPayload::Response(result) => {
            let tx = {
                let mut pending = inner.pending.lock().await;
                pending.remove(&request_id)
            };
            if let Some(tx) = tx {
                let mapped = match result {
                    RpcResult::Ok { value } => Ok(value),
                    RpcResult::Err { error } if error.code == RpcErrorCode::Cancelled => {
                        Err(RpcError::Canceled)
                    }
                    RpcResult::Err { error } => Err(RpcError::Remote(error)),
                    RpcResult::Unknown => Err(RpcError::UnexpectedResponse),
                };
                let _ = tx.send(mapped);
            }
            Ok(())
        }
        RpcPayload::Request(request) => {
            let handler = inner.request_handler.read().unwrap().clone();
            let inner_clone = inner.clone();
            tokio::spawn(async move {
                let (cancel_tx, cancel_rx) = watch::channel(false);
                {
                    let mut map = inner_clone.incoming_cancels.lock().await;
                    map.insert(request_id, cancel_tx);
                }

                let ctx = RequestContext {
                    request_id,
                    cancel: CancellationToken { rx: cancel_rx },
                };

                let result = if let Some(handler) = handler {
                    handler(ctx, request).await
                } else {
                    Err(ProtoRpcError {
                        code: RpcErrorCode::InvalidRequest,
                        message: "no request handler installed".into(),
                        retryable: false,
                        details: None,
                    })
                };

                {
                    let mut map = inner_clone.incoming_cancels.lock().await;
                    map.remove(&request_id);
                }

                let payload = match result {
                    Ok(value) => RpcPayload::Response(RpcResult::Ok { value }),
                    Err(error) => RpcPayload::Response(RpcResult::Err { error }),
                };

                if let Err(err) = send_rpc_payload(&inner_clone, request_id, payload).await {
                    inner_clone.close(err).await;
                }
            });
            Ok(())
        }
        RpcPayload::Notification(notification) => {
            let handler = inner.notification_handler.read().unwrap().clone();
            if let Some(handler) = handler {
                tokio::spawn(async move {
                    handler(notification).await;
                });
            }
            Ok(())
        }
        RpcPayload::Cancel => {
            if inner.capabilities.supports_cancel {
                {
                    let map = inner.incoming_cancels.lock().await;
                    if let Some(tx) = map.get(&request_id) {
                        let _ = tx.send(true);
                    }
                }

                let pending = {
                    let mut pending = inner.pending.lock().await;
                    pending.remove(&request_id)
                };
                if let Some(tx) = pending {
                    let _ = tx.send(Err(RpcError::Canceled));
                }

                let handler = inner.cancel_handler.read().unwrap().clone();
                if let Some(handler) = handler {
                    handler(request_id);
                }
            }
            Ok(())
        }
        RpcPayload::Unknown => Ok(()),
    }
}

async fn send_rpc_payload(
    inner: &Arc<Inner>,
    request_id: RequestId,
    payload: RpcPayload,
) -> Result<(), RpcTransportError> {
    if inner.is_closed().await.is_some() {
        return Err(RpcTransportError::ConnectionClosed);
    }

    let uncompressed = v3::encode_rpc_payload(&payload).map_err(|err| RpcTransportError::EncodeError {
        message: err.to_string(),
    })?;
    let max_packet_len = inner.capabilities.max_packet_len as usize;
    if uncompressed.len() > max_packet_len {
        return Err(RpcTransportError::PacketTooLarge {
            len: uncompressed.len(),
            max: max_packet_len,
        });
    }

    let (compression, wire_bytes) = maybe_compress(&inner.capabilities, inner.compression_threshold, &uncompressed)?;

    if wire_bytes.len() > max_packet_len {
        return Err(RpcTransportError::PacketTooLarge {
            len: wire_bytes.len(),
            max: max_packet_len,
        });
    }

    // First attempt: single Packet frame.
    let packet_frame = WireFrame::Packet {
        id: request_id,
        compression,
        data: wire_bytes.clone(),
    };
    let encoded_packet = v3::encode_wire_frame(&packet_frame).map_err(|err| RpcTransportError::EncodeError {
        message: err.to_string(),
    })?;

    if encoded_packet.len() <= inner.capabilities.max_frame_len as usize {
        inner
            .tx
            .send(Bytes::from(encoded_packet))
            .await
            .map_err(|_| RpcTransportError::ConnectionClosed)?;
        return Ok(());
    }

    if !inner.capabilities.supports_chunking {
        return Err(RpcTransportError::FrameTooLarge {
            len: encoded_packet
                .len()
                .min(u32::MAX as usize) as u32,
            max: inner.capabilities.max_frame_len,
        });
    }

    let bytes = Bytes::from(wire_bytes);
    let max_frame_len = inner.capabilities.max_frame_len as usize;
    let mut offset = 0usize;
    let mut seq: u32 = 0;
    let mut base_chunk = max_frame_len.saturating_sub(CHUNK_OVERHEAD_GUESS).max(1);

    while offset < bytes.len() {
        let remaining = bytes.len() - offset;
        let mut take = remaining.min(base_chunk);

        // Ensure the encoded chunk frame fits within max_frame_len.
        let encoded = loop {
            if take == 0 {
                return Err(RpcTransportError::ProtocolViolation {
                    message: "unable to fit packet chunk in negotiated max_frame_len".into(),
                });
            }
            let last = offset + take == bytes.len();
            let frame = WireFrame::PacketChunk {
                id: request_id,
                compression,
                seq,
                last,
                data: bytes.slice(offset..offset + take).to_vec(),
            };
            let encoded = v3::encode_wire_frame(&frame).map_err(|err| RpcTransportError::EncodeError {
                message: err.to_string(),
            })?;
            if encoded.len() <= max_frame_len {
                break encoded;
            }
            if take <= 1 {
                return Err(RpcTransportError::FrameTooLarge {
                    len: encoded.len().min(u32::MAX as usize) as u32,
                    max: inner.capabilities.max_frame_len,
                });
            }
            // Reduce a bit and try again.
            take = take.saturating_sub(128).max(1);
            base_chunk = base_chunk.min(take);
        };

        inner
            .tx
            .send(Bytes::from(encoded))
            .await
            .map_err(|_| RpcTransportError::ConnectionClosed)?;

        offset += take;
        seq = seq.wrapping_add(1);
    }

    Ok(())
}

fn maybe_compress(
    negotiated: &Capabilities,
    threshold: usize,
    uncompressed: &[u8],
) -> Result<(CompressionAlgo, Vec<u8>), RpcTransportError> {
    let max_packet_len = negotiated.max_packet_len as usize;
    if uncompressed.len() > max_packet_len {
        return Err(RpcTransportError::PacketTooLarge {
            len: uncompressed.len(),
            max: max_packet_len,
        });
    }

    let allow_zstd = negotiated
        .supported_compression
        .iter()
        .any(|algo| *algo == CompressionAlgo::Zstd);

    #[cfg(not(feature = "zstd"))]
    let _ = threshold;

    #[cfg(feature = "zstd")]
    if allow_zstd && uncompressed.len() >= threshold {
        let compressed = zstd::bulk::compress(uncompressed, 3).map_err(|err| {
            RpcTransportError::EncodeError {
                message: format!("zstd compress failed: {err}"),
            }
        })?;
        if compressed.len() < uncompressed.len() {
            return Ok((CompressionAlgo::Zstd, compressed));
        }
    }

    #[cfg(not(feature = "zstd"))]
    if allow_zstd {
        // Negotiated Zstd but local build doesn't support it.
        // We'll fall back to `None` for outbound packets.
    }

    let mut out = Vec::new();
    out.try_reserve_exact(uncompressed.len()).map_err(|err| {
        RpcTransportError::AllocationFailed {
            message: format!("allocate packet buffer ({} bytes): {err}", uncompressed.len()),
        }
    })?;
    out.extend_from_slice(uncompressed);
    Ok((CompressionAlgo::None, out))
}

fn maybe_decompress(
    negotiated: &Capabilities,
    compression: CompressionAlgo,
    data: &[u8],
) -> Result<Vec<u8>, RpcTransportError> {
    let max_packet_len = negotiated.max_packet_len as usize;

    if !negotiated.supported_compression.contains(&compression)
        && compression != CompressionAlgo::None
    {
        return Err(RpcTransportError::UnsupportedCompression { algo: compression });
    }

    match compression {
        CompressionAlgo::None => {
            if data.len() > max_packet_len {
                return Err(RpcTransportError::PacketTooLarge {
                    len: data.len(),
                    max: max_packet_len,
                });
            }
            let mut out = Vec::new();
            out.try_reserve_exact(data.len()).map_err(|err| {
                RpcTransportError::AllocationFailed {
                    message: format!("allocate packet buffer ({} bytes): {err}", data.len()),
                }
            })?;
            out.extend_from_slice(data);
            Ok(out)
        }
        CompressionAlgo::Zstd => {
            #[cfg(feature = "zstd")]
            {
                return decompress_zstd_with_limit(data, max_packet_len);
            }
            #[cfg(not(feature = "zstd"))]
            {
                return Err(RpcTransportError::UnsupportedCompression { algo: compression });
            }
        }
        CompressionAlgo::Unknown => Err(RpcTransportError::UnsupportedCompression { algo: compression }),
    }
}

#[cfg(feature = "zstd")]
fn decompress_zstd_with_limit(data: &[u8], limit: usize) -> Result<Vec<u8>, RpcTransportError> {
    use std::io::Read;

    let mut decoder = zstd::stream::read::Decoder::new(data).map_err(|err| RpcTransportError::DecodeError {
        message: format!("create zstd decoder: {err}"),
    })?;
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = decoder.read(&mut buf).map_err(|err| RpcTransportError::DecodeError {
            message: format!("read zstd stream: {err}"),
        })?;
        if n == 0 {
            break;
        }
        if out.len() + n > limit {
            return Err(RpcTransportError::PacketTooLarge {
                len: out.len() + n,
                max: limit,
            });
        }
        out.try_reserve(n).map_err(|err| RpcTransportError::AllocationFailed {
            message: format!(
                "allocate decompression buffer ({} bytes): {err}",
                out.len() + n
            ),
        })?;
        out.extend_from_slice(&buf[..n]);
    }
    Ok(out)
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

#[cfg(test)]
mod tests {
    use super::*;

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
            use tokio::io::AsyncWriteExt as _;

            let max_frame_len = 1024u32;
            let len = u32::MAX;

            let mut bytes = Vec::new();
            bytes.extend_from_slice(&len.to_le_bytes());

            let (mut tx, mut rx) = tokio::io::duplex(bytes.len());
            tx.write_all(&bytes).await.expect("write prefix");
            drop(tx);

            let err = tokio::time::timeout(
                std::time::Duration::from_millis(100),
                read_wire_frame(&mut rx, max_frame_len),
            )
            .await
            .expect("read_wire_frame timed out")
            .expect_err("expected oversize frame error");

            assert!(matches!(err, RpcTransportError::FrameTooLarge { .. }));
        });
    }
}
