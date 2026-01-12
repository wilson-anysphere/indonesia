use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{FileText, Revision, ShardId, ShardIndex, WorkerId, WorkerStats, MAX_MESSAGE_BYTES};

pub const PROTOCOL_MAJOR: u32 = 3;
pub const PROTOCOL_MINOR: u32 = 0;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProtocolVersion {
    pub major: u32,
    pub minor: u32,
}

impl ProtocolVersion {
    pub const CURRENT: ProtocolVersion = ProtocolVersion {
        major: PROTOCOL_MAJOR,
        minor: PROTOCOL_MINOR,
    };
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SupportedVersions {
    pub min: ProtocolVersion,
    pub max: ProtocolVersion,
}

impl SupportedVersions {
    pub fn supports(&self, version: ProtocolVersion) -> bool {
        self.min <= version && version <= self.max
    }

    pub fn choose_common(&self, other: &SupportedVersions) -> Option<ProtocolVersion> {
        let min = std::cmp::max(self.min, other.min);
        let max = std::cmp::min(self.max, other.max);
        (min <= max).then_some(max)
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompressionAlgo {
    None,
    Zstd,
    #[serde(other)]
    Unknown,
}

pub const DEFAULT_MAX_FRAME_LEN: u32 = 64 * 1024 * 1024;
pub const DEFAULT_MAX_PACKET_LEN: u32 = 64 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Capabilities {
    pub max_frame_len: u32,
    pub max_packet_len: u32,
    /// Ordered by preference (best-first).
    pub supported_compression: Vec<CompressionAlgo>,
    pub supports_cancel: bool,
    pub supports_chunking: bool,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            max_frame_len: DEFAULT_MAX_FRAME_LEN,
            max_packet_len: DEFAULT_MAX_PACKET_LEN,
            supported_compression: vec![CompressionAlgo::None],
            supports_cancel: false,
            supports_chunking: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CachedIndexInfo {
    pub revision: Revision,
    pub index_generation: u64,
    pub symbol_count: u32,
}

impl CachedIndexInfo {
    pub fn from_index(index: &ShardIndex) -> Self {
        Self {
            revision: index.revision,
            index_generation: index.index_generation,
            symbol_count: index.symbols.len().try_into().unwrap_or(u32::MAX),
        }
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkerHello {
    pub shard_id: ShardId,
    pub auth_token: Option<String>,
    pub supported_versions: SupportedVersions,
    pub capabilities: Capabilities,
    pub cached_index_info: Option<CachedIndexInfo>,
    pub worker_build: Option<String>,
}

impl fmt::Debug for WorkerHello {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkerHello")
            .field("shard_id", &self.shard_id)
            .field("auth_present", &self.auth_token.is_some())
            .field("supported_versions", &self.supported_versions)
            .field("capabilities", &self.capabilities)
            .field("cached_index_info", &self.cached_index_info)
            .field("worker_build", &self.worker_build)
            .finish()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RouterWelcome {
    pub worker_id: WorkerId,
    pub shard_id: ShardId,
    pub revision: Revision,
    pub chosen_version: ProtocolVersion,
    pub chosen_capabilities: Capabilities,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RejectCode {
    InvalidRequest,
    Unauthorized,
    UnsupportedVersion,
    Internal,
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HandshakeReject {
    pub code: RejectCode,
    pub message: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RpcErrorCode {
    InvalidRequest,
    Unauthorized,
    UnsupportedVersion,
    TooLarge,
    Cancelled,
    Internal,
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RpcError {
    pub code: RpcErrorCode,
    pub message: String,
    pub retryable: bool,
    pub details: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RpcResult<T> {
    Ok {
        value: T,
    },
    Err {
        error: RpcError,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    LoadFiles {
        revision: Revision,
        files: Vec<FileText>,
    },
    IndexShard {
        revision: Revision,
        files: Vec<FileText>,
    },
    UpdateFile {
        revision: Revision,
        file: FileText,
    },
    /// Best-effort diagnostics for a single file.
    ///
    /// This is intentionally minimal: it exists to enable an end-to-end distributed analysis
    /// prototype. Callers should treat failures as non-fatal.
    Diagnostics {
        #[serde(deserialize_with = "crate::bounded_de::small_string")]
        path: String,
    },
    GetWorkerStats,
    Shutdown,
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Info,
    Hint,
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteDiagnostic {
    pub severity: DiagnosticSeverity,
    pub line: u32,
    pub column: u32,
    #[serde(deserialize_with = "crate::bounded_de::small_string")]
    pub message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", content = "body", rename_all = "snake_case")]
pub enum Response {
    Ack,
    ShardIndex(ShardIndex),
    Diagnostics {
        #[serde(deserialize_with = "crate::bounded_de::diagnostics_vec")]
        diagnostics: Vec<RemoteDiagnostic>,
    },
    WorkerStats(WorkerStats),
    Shutdown,
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", content = "body", rename_all = "snake_case")]
pub enum Notification {
    CachedIndex(ShardIndex),
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", content = "body", rename_all = "snake_case")]
pub enum RpcPayload {
    Request(Request),
    Response(RpcResult<Response>),
    Notification(Notification),
    Cancel,
    #[serde(other)]
    Unknown,
}

mod cbor_bytes {
    use serde::de::{Error, SeqAccess, Visitor};
    use serde::{Deserializer, Serializer};

    pub fn serialize<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_bytes(bytes)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct BytesVisitor;

        impl<'de> Visitor<'de> for BytesVisitor {
            type Value = Vec<u8>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a CBOR byte string (or a sequence of u8)")
            }

            fn visit_bytes<E>(self, v: &[u8]) -> Result<Self::Value, E>
            where
                E: Error,
            {
                Ok(v.to_vec())
            }

            fn visit_byte_buf<E>(self, v: Vec<u8>) -> Result<Self::Value, E>
            where
                E: Error,
            {
                Ok(v)
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut out = Vec::new();
                while let Some(byte) = seq.next_element::<u8>()? {
                    out.push(byte);
                }
                Ok(out)
            }
        }

        deserializer.deserialize_any(BytesVisitor)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", content = "body", rename_all = "snake_case")]
pub enum WireFrame {
    Hello(WorkerHello),
    Welcome(RouterWelcome),
    Reject(HandshakeReject),
    Packet {
        id: u64,
        compression: CompressionAlgo,
        #[serde(with = "cbor_bytes")]
        data: Vec<u8>,
    },
    PacketChunk {
        id: u64,
        compression: CompressionAlgo,
        seq: u32,
        last: bool,
        #[serde(with = "cbor_bytes")]
        data: Vec<u8>,
    },
    #[serde(other)]
    Unknown,
}

pub fn encode_wire_frame(frame: &WireFrame) -> anyhow::Result<Vec<u8>> {
    Ok(serde_cbor::to_vec(frame)?)
}

pub fn decode_wire_frame(bytes: &[u8]) -> anyhow::Result<WireFrame> {
    anyhow::ensure!(
        bytes.len() <= MAX_MESSAGE_BYTES,
        "wire frame too large: {} bytes (max {})",
        bytes.len(),
        MAX_MESSAGE_BYTES
    );
    crate::validate_cbor::validate_cbor(bytes)?;
    Ok(serde_cbor::from_slice(bytes)?)
}

pub fn encode_rpc_payload(payload: &RpcPayload) -> anyhow::Result<Vec<u8>> {
    Ok(serde_cbor::to_vec(payload)?)
}

pub fn decode_rpc_payload(bytes: &[u8]) -> anyhow::Result<RpcPayload> {
    anyhow::ensure!(
        bytes.len() <= MAX_MESSAGE_BYTES,
        "rpc payload too large: {} bytes (max {})",
        bytes.len(),
        MAX_MESSAGE_BYTES
    );
    crate::validate_cbor::validate_cbor(bytes)?;
    Ok(serde_cbor::from_slice(bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_hello_debug_does_not_expose_auth_token() {
        let token = "super-secret-token";
        let hello = WorkerHello {
            shard_id: 1,
            auth_token: Some(token.to_string()),
            supported_versions: SupportedVersions {
                min: ProtocolVersion::CURRENT,
                max: ProtocolVersion::CURRENT,
            },
            capabilities: Capabilities::default(),
            cached_index_info: None,
            worker_build: None,
        };

        let output = format!("{hello:?}");
        assert!(
            !output.contains(token),
            "WorkerHello debug output leaked auth token: {output}"
        );
        assert!(
            output.contains("auth_present"),
            "WorkerHello debug output should include auth presence indicator: {output}"
        );
    }
}
