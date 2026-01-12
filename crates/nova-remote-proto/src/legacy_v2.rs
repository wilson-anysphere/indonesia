use std::fmt;

use anyhow::{anyhow, bail, ensure, Context};
use serde::{Deserialize, Serialize};

use crate::{
    FileText, Revision, ScoredSymbol, ShardId, ShardIndexInfo, WorkerId, WorkerStats,
    MAX_FILES_PER_MESSAGE, MAX_FILE_TEXT_BYTES, MAX_MESSAGE_BYTES, MAX_SEARCH_RESULTS_PER_MESSAGE,
    MAX_SMALL_STRING_BYTES,
};

pub const PROTOCOL_VERSION: u32 = 4;

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum RpcMessage {
    /// First message sent by the worker on connect.
    WorkerHello {
        shard_id: ShardId,
        #[serde(deserialize_with = "crate::bounded_de::opt_small_string")]
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
        #[serde(deserialize_with = "crate::bounded_de::files_vec")]
        files: Vec<FileText>,
    },

    /// Build (or rebuild) the shard index from a full file snapshot.
    IndexShard {
        revision: Revision,
        #[serde(deserialize_with = "crate::bounded_de::files_vec")]
        files: Vec<FileText>,
    },
    /// Update a single file in the shard and rebuild affected indexes (MVP: rebuild shard).
    UpdateFile { revision: Revision, file: FileText },

    /// Query worker internal counters (used by tests + monitoring).
    GetWorkerStats,

    /// Response to `GetWorkerStats`.
    WorkerStats(WorkerStats),
    /// Response to `IndexShard`/`UpdateFile`.
    ShardIndexInfo(ShardIndexInfo),

    /// Query a shard's symbol index and return the top-k results.
    SearchSymbols {
        #[serde(deserialize_with = "crate::bounded_de::small_string")]
        query: String,
        limit: u32,
    },

    /// Response to `SearchSymbols`.
    SearchSymbolsResult {
        #[serde(deserialize_with = "crate::bounded_de::search_results_vec")]
        items: Vec<ScoredSymbol>,
    },

    /// Generic success response for commands that don't have a structured payload.
    Ack,

    /// Request graceful shutdown.
    Shutdown,

    Error {
        #[serde(deserialize_with = "crate::bounded_de::small_string")]
        message: String,
    },
}

impl fmt::Debug for RpcMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RpcMessage::WorkerHello {
                shard_id,
                auth_token,
                has_cached_index,
            } => f
                .debug_struct("WorkerHello")
                .field("shard_id", shard_id)
                .field("auth_present", &auth_token.is_some())
                .field("has_cached_index", has_cached_index)
                .finish(),
            RpcMessage::RouterHello {
                worker_id,
                shard_id,
                revision,
                protocol_version,
            } => f
                .debug_struct("RouterHello")
                .field("worker_id", worker_id)
                .field("shard_id", shard_id)
                .field("revision", revision)
                .field("protocol_version", protocol_version)
                .finish(),
            RpcMessage::LoadFiles { revision, files } => f
                .debug_struct("LoadFiles")
                .field("revision", revision)
                .field("files", files)
                .finish(),
            RpcMessage::IndexShard { revision, files } => f
                .debug_struct("IndexShard")
                .field("revision", revision)
                .field("files", files)
                .finish(),
            RpcMessage::UpdateFile { revision, file } => f
                .debug_struct("UpdateFile")
                .field("revision", revision)
                .field("file", file)
                .finish(),
            RpcMessage::GetWorkerStats => f.write_str("GetWorkerStats"),
            RpcMessage::WorkerStats(stats) => f.debug_tuple("WorkerStats").field(stats).finish(),
            RpcMessage::ShardIndexInfo(info) => {
                f.debug_tuple("ShardIndexInfo").field(info).finish()
            }
            RpcMessage::SearchSymbols { query, limit } => f
                .debug_struct("SearchSymbols")
                .field("query", query)
                .field("limit", limit)
                .finish(),
            RpcMessage::SearchSymbolsResult { items } => f
                .debug_struct("SearchSymbolsResult")
                .field("items", items)
                .finish(),
            RpcMessage::Ack => f.write_str("Ack"),
            RpcMessage::Shutdown => f.write_str("Shutdown"),
            RpcMessage::Error { message } => {
                f.debug_struct("Error").field("message", message).finish()
            }
        }
    }
}

pub fn encode_message(msg: &RpcMessage) -> anyhow::Result<Vec<u8>> {
    let mut w = WireWriter::new();
    w.write_rpc_message(msg)?;
    w.finish()
}

pub fn decode_message(bytes: &[u8]) -> anyhow::Result<RpcMessage> {
    ensure!(
        bytes.len() <= MAX_MESSAGE_BYTES,
        "rpc payload too large: {} bytes (max {})",
        bytes.len(),
        MAX_MESSAGE_BYTES
    );

    let mut r = WireReader::new(bytes);
    let msg = r
        .read_rpc_message()
        .context("decode legacy_v2::RpcMessage")?;
    ensure!(
        r.is_empty(),
        "trailing {} bytes after legacy_v2::RpcMessage",
        r.remaining()
    );
    Ok(msg)
}

/// Legacy wire encoding for `legacy_v2::RpcMessage`.
///
/// This intentionally avoids `bincode` because its serde-backed decoding can allocate based on
/// attacker-controlled length prefixes before validating that the buffer contains enough bytes.
/// A tiny frame could therefore trigger huge allocations during deserialization.
///
/// The encoding below is a compact fixed-width format with explicit, defensive limits. All
/// variable-size fields (strings/vecs) are validated against:
/// - a hard cap (see `crate::MAX_*` constants), and
/// - the remaining bytes in the input buffer (to prevent allocation bombs).
struct WireWriter {
    buf: Vec<u8>,
}

impl WireWriter {
    fn new() -> Self {
        Self { buf: Vec::new() }
    }

    fn finish(self) -> anyhow::Result<Vec<u8>> {
        ensure!(
            self.buf.len() <= MAX_MESSAGE_BYTES,
            "encoded rpc payload too large: {} bytes (max {})",
            self.buf.len(),
            MAX_MESSAGE_BYTES
        );
        Ok(self.buf)
    }

    fn write_u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    fn write_bool(&mut self, v: bool) {
        self.write_u8(if v { 1 } else { 0 });
    }

    fn write_u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    fn write_u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    fn write_i32(&mut self, v: i32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    fn write_len(&mut self, len: usize) -> anyhow::Result<()> {
        let len_u32: u32 = len
            .try_into()
            .map_err(|_| anyhow!("length does not fit in u32: {len}"))?;
        self.write_u32(len_u32);
        Ok(())
    }

    fn write_bytes(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    fn write_string(
        &mut self,
        field: &'static str,
        s: &str,
        max_bytes: usize,
    ) -> anyhow::Result<()> {
        let len = s.len();
        ensure!(
            len <= max_bytes,
            "{field} too large: {len} bytes (max {max_bytes})"
        );
        self.write_len(len)?;
        self.write_bytes(s.as_bytes());
        Ok(())
    }

    fn write_option<T>(
        &mut self,
        opt: Option<&T>,
        mut write: impl FnMut(&mut Self, &T) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        match opt {
            None => self.write_u8(0),
            Some(value) => {
                self.write_u8(1);
                write(self, value)?;
            }
        }
        Ok(())
    }

    fn write_vec<T>(
        &mut self,
        field: &'static str,
        values: &[T],
        max_len: usize,
        mut write: impl FnMut(&mut Self, &T) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        ensure!(
            values.len() <= max_len,
            "{field} too long: {} elements (max {max_len})",
            values.len()
        );
        self.write_len(values.len())?;
        for value in values {
            write(self, value)?;
        }
        Ok(())
    }

    fn write_file_text(&mut self, file: &FileText) -> anyhow::Result<()> {
        self.write_string("FileText.path", &file.path, MAX_SMALL_STRING_BYTES)?;
        self.write_string("FileText.text", &file.text, MAX_FILE_TEXT_BYTES)?;
        Ok(())
    }

    fn write_symbol_rank_key(&mut self, key: &crate::SymbolRankKey) {
        self.write_i32(key.kind_rank);
        self.write_i32(key.score);
    }

    fn write_scored_symbol(&mut self, sym: &ScoredSymbol) -> anyhow::Result<()> {
        self.write_string("ScoredSymbol.name", &sym.name, MAX_SMALL_STRING_BYTES)?;
        self.write_string("ScoredSymbol.path", &sym.path, MAX_SMALL_STRING_BYTES)?;
        self.write_symbol_rank_key(&sym.rank_key);
        Ok(())
    }

    fn write_worker_stats(&mut self, stats: &WorkerStats) {
        self.write_u32(stats.shard_id);
        self.write_u64(stats.revision);
        self.write_u64(stats.index_generation);
        self.write_u32(stats.file_count);
    }

    fn write_shard_index_info(&mut self, info: &ShardIndexInfo) {
        self.write_u32(info.shard_id);
        self.write_u64(info.revision);
        self.write_u64(info.index_generation);
        self.write_u32(info.symbol_count);
    }

    fn write_rpc_message(&mut self, msg: &RpcMessage) -> anyhow::Result<()> {
        match msg {
            RpcMessage::WorkerHello {
                shard_id,
                auth_token,
                has_cached_index,
            } => {
                self.write_u8(0);
                self.write_u32(*shard_id);
                self.write_option(auth_token.as_ref(), |w, token| {
                    w.write_string("WorkerHello.auth_token", token, MAX_SMALL_STRING_BYTES)
                })?;
                self.write_bool(*has_cached_index);
            }
            RpcMessage::RouterHello {
                worker_id,
                shard_id,
                revision,
                protocol_version,
            } => {
                self.write_u8(1);
                self.write_u32(*worker_id);
                self.write_u32(*shard_id);
                self.write_u64(*revision);
                self.write_u32(*protocol_version);
            }
            RpcMessage::LoadFiles { revision, files } => {
                self.write_u8(2);
                self.write_u64(*revision);
                self.write_vec(
                    "LoadFiles.files",
                    files,
                    MAX_FILES_PER_MESSAGE,
                    Self::write_file_text,
                )?;
            }
            RpcMessage::IndexShard { revision, files } => {
                self.write_u8(3);
                self.write_u64(*revision);
                self.write_vec(
                    "IndexShard.files",
                    files,
                    MAX_FILES_PER_MESSAGE,
                    Self::write_file_text,
                )?;
            }
            RpcMessage::UpdateFile { revision, file } => {
                self.write_u8(4);
                self.write_u64(*revision);
                self.write_file_text(file)?;
            }
            RpcMessage::GetWorkerStats => self.write_u8(5),
            RpcMessage::WorkerStats(stats) => {
                self.write_u8(6);
                self.write_worker_stats(stats);
            }
            RpcMessage::ShardIndexInfo(info) => {
                self.write_u8(7);
                self.write_shard_index_info(info);
            }
            RpcMessage::SearchSymbols { query, limit } => {
                self.write_u8(8);
                self.write_string("SearchSymbols.query", query, MAX_SMALL_STRING_BYTES)?;
                ensure!(
                    (*limit as usize) <= MAX_SEARCH_RESULTS_PER_MESSAGE,
                    "SearchSymbols.limit too large: {limit} (max {MAX_SEARCH_RESULTS_PER_MESSAGE})"
                );
                self.write_u32(*limit);
            }
            RpcMessage::SearchSymbolsResult { items } => {
                self.write_u8(9);
                self.write_vec(
                    "SearchSymbolsResult.items",
                    items,
                    MAX_SEARCH_RESULTS_PER_MESSAGE,
                    Self::write_scored_symbol,
                )?;
            }
            RpcMessage::Ack => self.write_u8(10),
            RpcMessage::Shutdown => self.write_u8(11),
            RpcMessage::Error { message } => {
                self.write_u8(12);
                self.write_string("Error.message", message, MAX_SMALL_STRING_BYTES)?;
            }
        }
        Ok(())
    }
}

struct WireReader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> WireReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.offset)
    }

    fn is_empty(&self) -> bool {
        self.offset >= self.bytes.len()
    }

    fn read_exact(&mut self, len: usize) -> anyhow::Result<&'a [u8]> {
        ensure!(
            self.remaining() >= len,
            "unexpected EOF: need {len} bytes, have {}",
            self.remaining()
        );
        let start = self.offset;
        self.offset += len;
        Ok(&self.bytes[start..start + len])
    }

    fn read_u8(&mut self) -> anyhow::Result<u8> {
        Ok(self.read_exact(1)?[0])
    }

    fn read_bool(&mut self) -> anyhow::Result<bool> {
        match self.read_u8()? {
            0 => Ok(false),
            1 => Ok(true),
            other => bail!("invalid bool tag: {other}"),
        }
    }

    fn read_u32(&mut self) -> anyhow::Result<u32> {
        let bytes = self.read_exact(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn read_u64(&mut self) -> anyhow::Result<u64> {
        let bytes = self.read_exact(8)?;
        Ok(u64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    fn read_i32(&mut self) -> anyhow::Result<i32> {
        let bytes = self.read_exact(4)?;
        Ok(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn read_len(&mut self) -> anyhow::Result<usize> {
        Ok(self.read_u32()? as usize)
    }

    fn read_string(&mut self, field: &'static str, max_bytes: usize) -> anyhow::Result<String> {
        let len = self
            .read_len()
            .with_context(|| format!("read {field} length"))?;
        ensure!(
            len <= max_bytes,
            "{field} too large: {len} bytes (max {max_bytes})"
        );
        ensure!(
            len <= self.remaining(),
            "unexpected EOF while reading {field}: need {len} bytes, have {}",
            self.remaining()
        );
        let bytes = self.read_exact(len)?;
        let s = std::str::from_utf8(bytes).with_context(|| format!("invalid UTF-8 in {field}"))?;
        let mut out = String::new();
        out.try_reserve_exact(s.len())
            .map_err(|err| anyhow!("failed to reserve {} bytes for {field}: {err:?}", s.len()))?;
        out.push_str(s);
        Ok(out)
    }

    fn read_option<T>(
        &mut self,
        mut read: impl FnMut(&mut Self) -> anyhow::Result<T>,
    ) -> anyhow::Result<Option<T>> {
        match self.read_u8()? {
            0 => Ok(None),
            1 => Ok(Some(read(self)?)),
            other => bail!("invalid option tag: {other}"),
        }
    }

    fn read_vec<T>(
        &mut self,
        field: &'static str,
        max_len: usize,
        min_wire_bytes_per_item: usize,
        mut read: impl FnMut(&mut Self) -> anyhow::Result<T>,
    ) -> anyhow::Result<Vec<T>> {
        let len = self
            .read_len()
            .with_context(|| format!("read {field} length"))?;
        ensure!(
            len <= max_len,
            "{field} too long: {len} elements (max {max_len})"
        );

        let max_by_remaining = self
            .remaining()
            .checked_div(min_wire_bytes_per_item)
            .unwrap_or(0);
        ensure!(
            len <= max_by_remaining,
            "{field} length {len} exceeds remaining bytes (need at least {} bytes, have {})",
            len.saturating_mul(min_wire_bytes_per_item),
            self.remaining()
        );

        // Avoid `Vec::with_capacity(len)` here: even with length validation, allocation failure would
        // abort the process. `try_reserve_exact` lets us surface allocation failures as a normal
        // decode error instead.
        let mut out = Vec::new();
        out.try_reserve_exact(len)
            .map_err(|err| anyhow!("failed to reserve {len} elements for {field}: {err:?}"))?;
        for _ in 0..len {
            out.push(read(self)?);
        }
        Ok(out)
    }

    fn read_file_text(&mut self) -> anyhow::Result<FileText> {
        Ok(FileText {
            path: self.read_string("FileText.path", MAX_SMALL_STRING_BYTES)?,
            text: self.read_string("FileText.text", MAX_FILE_TEXT_BYTES)?,
        })
    }

    fn read_symbol_rank_key(&mut self) -> anyhow::Result<crate::SymbolRankKey> {
        Ok(crate::SymbolRankKey {
            kind_rank: self.read_i32().context("read SymbolRankKey.kind_rank")?,
            score: self.read_i32().context("read SymbolRankKey.score")?,
        })
    }

    fn read_scored_symbol(&mut self) -> anyhow::Result<ScoredSymbol> {
        Ok(ScoredSymbol {
            name: self.read_string("ScoredSymbol.name", MAX_SMALL_STRING_BYTES)?,
            path: self.read_string("ScoredSymbol.path", MAX_SMALL_STRING_BYTES)?,
            rank_key: self.read_symbol_rank_key()?,
        })
    }

    fn read_worker_stats(&mut self) -> anyhow::Result<WorkerStats> {
        Ok(WorkerStats {
            shard_id: self.read_u32().context("read WorkerStats.shard_id")?,
            revision: self.read_u64().context("read WorkerStats.revision")?,
            index_generation: self
                .read_u64()
                .context("read WorkerStats.index_generation")?,
            file_count: self.read_u32().context("read WorkerStats.file_count")?,
        })
    }

    fn read_shard_index_info(&mut self) -> anyhow::Result<ShardIndexInfo> {
        Ok(ShardIndexInfo {
            shard_id: self.read_u32().context("read ShardIndexInfo.shard_id")?,
            revision: self.read_u64().context("read ShardIndexInfo.revision")?,
            index_generation: self
                .read_u64()
                .context("read ShardIndexInfo.index_generation")?,
            symbol_count: self
                .read_u32()
                .context("read ShardIndexInfo.symbol_count")?,
        })
    }

    fn read_rpc_message(&mut self) -> anyhow::Result<RpcMessage> {
        let tag = self.read_u8().context("read legacy_v2::RpcMessage tag")?;
        match tag {
            0 => Ok(RpcMessage::WorkerHello {
                shard_id: self.read_u32().context("read WorkerHello.shard_id")?,
                auth_token: self.read_option(|r| {
                    r.read_string("WorkerHello.auth_token", MAX_SMALL_STRING_BYTES)
                })?,
                has_cached_index: self
                    .read_bool()
                    .context("read WorkerHello.has_cached_index")?,
            }),
            1 => Ok(RpcMessage::RouterHello {
                worker_id: self.read_u32().context("read RouterHello.worker_id")?,
                shard_id: self.read_u32().context("read RouterHello.shard_id")?,
                revision: self.read_u64().context("read RouterHello.revision")?,
                protocol_version: self
                    .read_u32()
                    .context("read RouterHello.protocol_version")?,
            }),
            2 => {
                let revision = self.read_u64().context("read LoadFiles.revision")?;
                let files = self.read_vec(
                    "LoadFiles.files",
                    MAX_FILES_PER_MESSAGE,
                    8, // u32 len + u32 len for the two strings (can be zero)
                    Self::read_file_text,
                )?;
                Ok(RpcMessage::LoadFiles { revision, files })
            }
            3 => {
                let revision = self.read_u64().context("read IndexShard.revision")?;
                let files = self.read_vec(
                    "IndexShard.files",
                    MAX_FILES_PER_MESSAGE,
                    8, // u32 len + u32 len for the two strings (can be zero)
                    Self::read_file_text,
                )?;
                Ok(RpcMessage::IndexShard { revision, files })
            }
            4 => Ok(RpcMessage::UpdateFile {
                revision: self.read_u64().context("read UpdateFile.revision")?,
                file: self.read_file_text().context("read UpdateFile.file")?,
            }),
            5 => Ok(RpcMessage::GetWorkerStats),
            6 => Ok(RpcMessage::WorkerStats(
                self.read_worker_stats().context("read WorkerStats")?,
            )),
            7 => Ok(RpcMessage::ShardIndexInfo(
                self.read_shard_index_info()
                    .context("read ShardIndexInfo")?,
            )),
            8 => {
                let query = self.read_string("SearchSymbols.query", MAX_SMALL_STRING_BYTES)?;
                let limit = self.read_u32().context("read SearchSymbols.limit")?;
                ensure!(
                    (limit as usize) <= MAX_SEARCH_RESULTS_PER_MESSAGE,
                    "SearchSymbols.limit too large: {limit} (max {MAX_SEARCH_RESULTS_PER_MESSAGE})"
                );
                Ok(RpcMessage::SearchSymbols { query, limit })
            }
            9 => Ok(RpcMessage::SearchSymbolsResult {
                items: self.read_vec(
                    "SearchSymbolsResult.items",
                    MAX_SEARCH_RESULTS_PER_MESSAGE,
                    16, // u32 len + u32 len + two i32 (can be zero-length strings)
                    Self::read_scored_symbol,
                )?,
            }),
            10 => Ok(RpcMessage::Ack),
            11 => Ok(RpcMessage::Shutdown),
            12 => Ok(RpcMessage::Error {
                message: self.read_string("Error.message", MAX_SMALL_STRING_BYTES)?,
            }),
            other => bail!("unknown legacy_v2::RpcMessage tag: {other}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_hello_debug_does_not_expose_auth_token() {
        let token = "super-secret-token";
        let msg = RpcMessage::WorkerHello {
            shard_id: 1,
            auth_token: Some(token.to_string()),
            has_cached_index: false,
        };

        let output = format!("{msg:?}");
        assert!(
            !output.contains(token),
            "RpcMessage::WorkerHello debug output leaked auth token: {output}"
        );
        assert!(
            output.contains("auth_present"),
            "RpcMessage::WorkerHello debug output should include auth presence indicator: {output}"
        );
    }
}
