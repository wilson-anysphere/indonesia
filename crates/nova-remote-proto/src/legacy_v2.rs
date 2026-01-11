use serde::{Deserialize, Serialize};

use crate::{FileText, Revision, ScoredSymbol, ShardId, ShardIndexInfo, WorkerId, WorkerStats};

pub const PROTOCOL_VERSION: u32 = 3;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum RpcMessage {
    /// First message sent by the worker on connect.
    WorkerHello {
        shard_id: ShardId,
        auth_token: Option<String>,
        /// Whether the worker was able to load a cached shard index on startup.
        ///
        /// When true, the router may send a `LoadFiles` message after connect to
        /// rehydrate the worker's in-memory file map so incremental `UpdateFile`
        /// operations can rebuild a complete index.
        has_cached_index: bool,
    },
    /// Acknowledge `WorkerHello`. The router assigns a stable `worker_id`.
    RouterHello {
        worker_id: WorkerId,
        shard_id: ShardId,
        revision: Revision,
        protocol_version: u32,
    },

    /// Load a full snapshot of the shard's files without changing the router's global view.
    ///
    /// This is used to rehydrate a worker's in-memory file map after a crash/restart so that
    /// subsequent `UpdateFile` messages can rebuild a complete shard index.
    LoadFiles {
        revision: Revision,
        files: Vec<FileText>,
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
    ShardIndexInfo(ShardIndexInfo),

    /// Query a shard's symbol index and return the top-k results.
    SearchSymbols { query: String, limit: u32 },

    /// Response to `SearchSymbols`.
    SearchSymbolsResult { items: Vec<ScoredSymbol> },

    /// Generic success response for commands that don't have a structured payload.
    Ack,

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
