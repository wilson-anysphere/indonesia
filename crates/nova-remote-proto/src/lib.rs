use serde::{Deserialize, Serialize};

/// Hard limits enforced during deserialization of untrusted network payloads.
///
/// These are intentionally conservative: they cap both the maximum frame size and the maximum
/// size/count of nested collections so a small input cannot trigger an outsized allocation.
///
/// NOTE: these limits are enforced in both the legacy lockstep codec (`legacy_v2`) and the v3 CBOR
/// decoder (via a non-allocating CBOR preflight validator).
/// Maximum size of a single RPC payload (not including the outer 4-byte length prefix).
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024; // 64 MiB

/// Maximum number of files allowed in a single `LoadFiles`/`IndexShard` request.
pub const MAX_FILES_PER_MESSAGE: usize = 100_000;

/// Maximum number of items allowed in a `SearchSymbolsResult` response (legacy lockstep protocol).
pub const MAX_SEARCH_RESULTS_PER_MESSAGE: usize = 10_000;

/// Maximum number of symbols allowed in a single `ShardIndex` message (v3 protocol).
pub const MAX_SYMBOLS_PER_SHARD_INDEX: usize = 1_000_000;

/// Maximum UTF-8 byte length of an individual file's contents (`FileText::text`).
pub const MAX_FILE_TEXT_BYTES: usize = 8 * 1024 * 1024; // 8 MiB

/// Maximum UTF-8 byte length for small identifier strings (paths, names, tokens, etc).
pub const MAX_SMALL_STRING_BYTES: usize = 16 * 1024; // 16 KiB

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

/// A stable, comparable rank key for fuzzy symbol search results.
///
/// This intentionally mirrors `nova_fuzzy::MatchScore::rank_key()` but is defined in the
/// remote protocol so routers can merge results from multiple workers without re-scoring.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct SymbolRankKey {
    pub kind_rank: i32,
    pub score: i32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScoredSymbol {
    pub name: String,
    pub path: String,
    pub rank_key: SymbolRankKey,
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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShardIndexInfo {
    pub shard_id: ShardId,
    pub revision: Revision,
    pub index_generation: u64,
    pub symbol_count: u32,
}

/// Legacy bincode protocol (`legacy_v2` module; length-delimited binary encoding, no request IDs/multiplexing).
pub mod legacy_v2;

/// v3 protocol: CBOR wire frames + request IDs/multiplexing, capabilities, errors.
pub mod v3;

mod validate_cbor;

pub use legacy_v2::{decode_message, encode_message, RpcMessage, PROTOCOL_VERSION};
