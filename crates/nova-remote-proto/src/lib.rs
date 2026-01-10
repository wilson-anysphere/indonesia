use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

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
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum RpcMessage {
    /// First message sent by the worker on connect.
    WorkerHello {
        shard_id: ShardId,
        auth_token: Option<String>,
        cached_index: Option<ShardIndex>,
    },
    /// Acknowledge `WorkerHello`. The router assigns a stable `worker_id`.
    RouterHello {
        worker_id: WorkerId,
        shard_id: ShardId,
        revision: Revision,
        protocol_version: u32,
    },

    /// Build (or rebuild) the shard index from a full file snapshot.
    IndexShard {
        revision: Revision,
        files: Vec<FileText>,
    },
    /// Update a single file in the shard and rebuild affected indexes (MVP: rebuild shard).
    UpdateFile {
        revision: Revision,
        file: FileText,
    },

    /// Query worker internal counters (used by tests + monitoring).
    GetWorkerStats,

    /// Response to `GetWorkerStats`.
    WorkerStats(WorkerStats),
    /// Response to `IndexShard`/`UpdateFile`.
    ShardIndex(ShardIndex),

    /// Request graceful shutdown.
    Shutdown,

    Error {
        message: String,
    },
}

pub fn encode_message(msg: &RpcMessage) -> anyhow::Result<Vec<u8>> {
    Ok(bincode::serialize(msg)?)
}

pub fn decode_message(bytes: &[u8]) -> anyhow::Result<RpcMessage> {
    Ok(bincode::deserialize(bytes)?)
}
