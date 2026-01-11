use std::future::Future;

use anyhow::{anyhow, Context, Result};
use nova_remote_proto::v3::{Capabilities, ProtocolVersion, SupportedVersions, WorkerHello};
use nova_remote_proto::{RpcMessage, ShardId, MAX_MESSAGE_BYTES};
use nova_remote_rpc::{RpcConnection, RpcTransportError};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// A connected test worker that has completed either the legacy v2 handshake or the v3 handshake.
pub enum ConnectedWorker<S> {
    LegacyV2(S),
    V3(RpcConnection),
}

impl<S> ConnectedWorker<S>
where
    S: AsyncWrite + Unpin,
{
    #[allow(dead_code)]
    pub async fn shutdown(self) {
        match self {
            ConnectedWorker::LegacyV2(mut stream) => {
                let _ = stream.shutdown().await;
            }
            ConnectedWorker::V3(conn) => {
                let _ = conn.shutdown().await;
            }
        }
    }
}

pub async fn connect_and_handshake_worker<S, F, Fut>(
    mut connect: F,
    shard_id: ShardId,
    auth_token: Option<String>,
) -> Result<ConnectedWorker<S>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<S>>,
{
    // First attempt: v3 handshake (post-migration router). If we get a clear UnsupportedVersion
    // error, fall back to the legacy v2 handshake (pre-migration router).
    let stream = connect().await?;
    let hello = WorkerHello {
        shard_id,
        auth_token: auth_token.clone(),
        supported_versions: SupportedVersions {
            min: ProtocolVersion::CURRENT,
            max: ProtocolVersion::CURRENT,
        },
        capabilities: Capabilities {
            supports_cancel: true,
            supports_chunking: true,
            ..Capabilities::default()
        },
        cached_index_info: None,
        worker_build: None,
    };

    match RpcConnection::handshake_as_worker(stream, hello).await {
        Ok((conn, welcome)) => {
            anyhow::ensure!(
                welcome.shard_id == shard_id,
                "welcome shard mismatch: expected {shard_id}, got {}",
                welcome.shard_id
            );
            Ok(ConnectedWorker::V3(conn))
        }
        Err(err) if should_fallback_to_legacy_v2(&err) => {
            let mut stream = connect().await?;
            legacy_v2_handshake(&mut stream, shard_id, auth_token).await?;
            Ok(ConnectedWorker::LegacyV2(stream))
        }
        Err(err) => Err(anyhow!("v3 handshake failed: {err}")),
    }
}

fn should_fallback_to_legacy_v2(err: &RpcTransportError) -> bool {
    match err {
        RpcTransportError::HandshakeFailed { message } => {
            message.contains("UnsupportedVersion") || message.contains("legacy_v2")
        }
        // Old routers may simply close or send non-v3 bytes.
        RpcTransportError::DecodeError { .. } | RpcTransportError::Io { .. } => true,
        _ => false,
    }
}

async fn legacy_v2_handshake(
    stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
    shard_id: ShardId,
    auth_token: Option<String>,
) -> Result<()> {
    write_v2_message(
        stream,
        &RpcMessage::WorkerHello {
            shard_id,
            auth_token,
            has_cached_index: false,
        },
    )
    .await?;

    let resp = read_v2_message_limited(stream, 64 * 1024).await?;
    match resp {
        RpcMessage::RouterHello {
            shard_id: ack_shard_id,
            protocol_version,
            ..
        } if ack_shard_id == shard_id && protocol_version == nova_remote_proto::PROTOCOL_VERSION => {
            Ok(())
        }
        RpcMessage::Error { message } => Err(anyhow!("{message}")),
        other => Err(anyhow!("unexpected legacy_v2 handshake response: {other:?}")),
    }
}

pub async fn write_v2_message(
    stream: &mut (impl AsyncWrite + Unpin),
    message: &RpcMessage,
) -> Result<()> {
    let payload = nova_remote_proto::encode_message(message)?;
    write_len_prefixed(stream, &payload).await
}

pub async fn read_v2_message_limited(
    stream: &mut (impl AsyncRead + Unpin),
    max_len: usize,
) -> Result<RpcMessage> {
    let payload = read_len_prefixed_limited(stream, max_len).await?;
    nova_remote_proto::decode_message(&payload)
}

async fn write_len_prefixed(stream: &mut (impl AsyncWrite + Unpin), payload: &[u8]) -> Result<()> {
    let len: u32 = payload
        .len()
        .try_into()
        .map_err(|_| anyhow!("payload too large"))?;
    stream.write_u32_le(len).await.context("write len")?;
    stream.write_all(payload).await.context("write payload")?;
    stream.flush().await.context("flush payload")?;
    Ok(())
}

async fn read_len_prefixed_limited(
    stream: &mut (impl AsyncRead + Unpin),
    max_len: usize,
) -> Result<Vec<u8>> {
    let len: usize = stream.read_u32_le().await.context("read len")?.try_into()?;
    let max_len = max_len.min(MAX_MESSAGE_BYTES);
    anyhow::ensure!(len <= max_len, "incoming payload too large ({len} bytes)");
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await.context("read payload")?;
    Ok(buf)
}
