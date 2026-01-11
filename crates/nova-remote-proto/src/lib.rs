use serde::{Deserialize, Serialize};

pub type Revision = u64;
pub type ShardId = u32;
pub type WorkerId = u32;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileText {
    pub path: String,
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub path: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShardIndex {
    pub shard_id: ShardId,
    pub revision: Revision,
    /// Monotonically increasing generation counter, local to the worker.
    pub index_generation: u64,
    pub symbols: Vec<Symbol>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkerStats {
    pub shard_id: ShardId,
    pub revision: Revision,
    pub index_generation: u64,
    pub file_count: u32,
}

/// Legacy v2 protocol (bincode-encoded, no request IDs/multiplexing).
pub mod legacy_v2;

/// v3 protocol: CBOR wire frames + request IDs/multiplexing, capabilities, errors.
pub mod v3;

pub use legacy_v2::{decode_message, encode_message, RpcMessage, PROTOCOL_VERSION};
