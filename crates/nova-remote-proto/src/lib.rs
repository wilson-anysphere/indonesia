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

/// Alias for the maximum payload length of the legacy `u32` length-prefixed transport.
pub const MAX_FRAME_BYTES: usize = MAX_MESSAGE_BYTES;

/// Maximum number of files allowed in a single `LoadFiles`/`IndexShard` request.
pub const MAX_FILES_PER_MESSAGE: usize = 100_000;

/// Maximum number of items allowed in a `SearchSymbolsResult` response (legacy lockstep protocol).
pub const MAX_SEARCH_RESULTS_PER_MESSAGE: usize = 10_000;

/// Maximum number of diagnostics allowed in a single v3 `Diagnostics` response.
pub const MAX_DIAGNOSTICS_PER_MESSAGE: usize = 100_000;

/// Maximum number of symbols allowed in a single `ShardIndex` message (v3 protocol).
pub const MAX_SYMBOLS_PER_SHARD_INDEX: usize = 1_000_000;

/// Maximum UTF-8 byte length of an individual file's contents (`FileText::text`).
pub const MAX_FILE_TEXT_BYTES: usize = 8 * 1024 * 1024; // 8 MiB

/// Maximum UTF-8 byte length for small identifier strings (paths, names, tokens, etc).
pub const MAX_SMALL_STRING_BYTES: usize = 16 * 1024; // 16 KiB

pub type Revision = u64;
pub type ShardId = u32;
pub type WorkerId = u32;

mod bounded_de {
    use std::fmt;
    use std::marker::PhantomData;

    use serde::de::{Deserialize, Deserializer, Error, SeqAccess, Visitor};

    use crate::{
        MAX_DIAGNOSTICS_PER_MESSAGE, MAX_FILES_PER_MESSAGE, MAX_FILE_TEXT_BYTES,
        MAX_SEARCH_RESULTS_PER_MESSAGE, MAX_SMALL_STRING_BYTES, MAX_SYMBOLS_PER_SHARD_INDEX,
    };

    const MAX_VEC_PREALLOC: usize = 1024;

    fn string_with_limit<'de, D>(
        deserializer: D,
        max_len: usize,
        what: &'static str,
    ) -> Result<String, D::Error>
    where
        D: Deserializer<'de>,
    {
        // Deserialize as `&str` first to avoid allocating based solely on an attacker-controlled
        // length prefix. This keeps decoding bounded by the input buffer size.
        let s: &'de str = Deserialize::deserialize(deserializer)?;
        if s.len() > max_len {
            return Err(D::Error::custom(format!(
                "{what} too large ({} bytes > {max_len})",
                s.len()
            )));
        }
        Ok(s.to_owned())
    }

    pub fn small_string<'de, D>(deserializer: D) -> Result<String, D::Error>
    where
        D: Deserializer<'de>,
    {
        string_with_limit(deserializer, MAX_SMALL_STRING_BYTES, "string")
    }

    pub fn file_text<'de, D>(deserializer: D) -> Result<String, D::Error>
    where
        D: Deserializer<'de>,
    {
        string_with_limit(deserializer, MAX_FILE_TEXT_BYTES, "file text")
    }

    pub fn opt_small_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s: Option<&'de str> = Option::deserialize(deserializer)?;
        match s {
            Some(s) => {
                if s.len() > MAX_SMALL_STRING_BYTES {
                    return Err(D::Error::custom(format!(
                        "string too large ({} bytes > {MAX_SMALL_STRING_BYTES})",
                        s.len()
                    )));
                }
                Ok(Some(s.to_owned()))
            }
            None => Ok(None),
        }
    }

    fn vec_with_limit<'de, D, T>(
        deserializer: D,
        max_len: usize,
        what: &'static str,
    ) -> Result<Vec<T>, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de>,
    {
        struct LimitedVecVisitor<T> {
            max_len: usize,
            what: &'static str,
            _marker: PhantomData<T>,
        }

        impl<'de, T> Visitor<'de> for LimitedVecVisitor<T>
        where
            T: Deserialize<'de>,
        {
            type Value = Vec<T>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, "a sequence")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                if let Some(hint) = seq.size_hint() {
                    if hint > self.max_len {
                        return Err(A::Error::custom(format!(
                            "{what} too long ({hint} items > {})",
                            self.max_len,
                            what = self.what
                        )));
                    }
                }

                let mut out = Vec::new();
                if let Some(hint) = seq.size_hint() {
                    // Prevent OOM from a hostile length prefix by capping preallocation.
                    out.reserve(hint.min(MAX_VEC_PREALLOC));
                }

                while let Some(value) = seq.next_element()? {
                    if out.len() == self.max_len {
                        return Err(A::Error::custom(format!(
                            "{what} too long (>{} items)",
                            self.max_len,
                            what = self.what
                        )));
                    }
                    out.push(value);
                }

                Ok(out)
            }
        }

        deserializer.deserialize_seq(LimitedVecVisitor {
            max_len,
            what,
            _marker: PhantomData,
        })
    }

    pub fn files_vec<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de>,
    {
        vec_with_limit(deserializer, MAX_FILES_PER_MESSAGE, "files")
    }

    pub fn search_results_vec<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de>,
    {
        vec_with_limit(
            deserializer,
            MAX_SEARCH_RESULTS_PER_MESSAGE,
            "search results",
        )
    }

    pub fn diagnostics_vec<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de>,
    {
        vec_with_limit(deserializer, MAX_DIAGNOSTICS_PER_MESSAGE, "diagnostics")
    }

    pub fn symbols_vec<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de>,
    {
        vec_with_limit(deserializer, MAX_SYMBOLS_PER_SHARD_INDEX, "symbols")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileText {
    #[serde(deserialize_with = "bounded_de::small_string")]
    pub path: String,
    #[serde(deserialize_with = "bounded_de::file_text")]
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Symbol {
    #[serde(deserialize_with = "bounded_de::small_string")]
    pub name: String,
    #[serde(deserialize_with = "bounded_de::small_string")]
    pub path: String,
    /// 0-based UTF-16 (LSP-compatible) position of the symbol's identifier.
    #[serde(default)]
    pub line: u32,
    /// 0-based UTF-16 (LSP-compatible) position of the symbol's identifier.
    #[serde(default)]
    pub column: u32,
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
    #[serde(deserialize_with = "bounded_de::small_string")]
    pub name: String,
    #[serde(deserialize_with = "bounded_de::small_string")]
    pub path: String,
    pub rank_key: SymbolRankKey,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShardIndex {
    pub shard_id: ShardId,
    pub revision: Revision,
    /// Monotonically increasing generation counter, local to the worker.
    pub index_generation: u64,
    #[serde(deserialize_with = "bounded_de::symbols_vec")]
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

/// Legacy lockstep protocol (`legacy_v2` module; length-delimited binary encoding, no request IDs/multiplexing).
pub mod legacy_v2;

/// v3 protocol: CBOR wire frames + request IDs/multiplexing, capabilities, errors.
pub mod v3;

mod validate_cbor;

pub use legacy_v2::{decode_message, encode_message, RpcMessage, PROTOCOL_VERSION};

pub mod transport {
    use anyhow::anyhow;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::sync::OnceLock;

    #[cfg(feature = "tokio")]
    use anyhow::Context;

    use crate::{decode_message, encode_message, RpcMessage, MAX_FRAME_BYTES};

    pub const LEN_PREFIX_BYTES: usize = 4;

    const DEFAULT_MAX_FRAME_BYTES: usize = 32 * 1024 * 1024; // 32 MiB
    const MAX_MESSAGE_SIZE_ENV_VAR: &str = "NOVA_RPC_MAX_MESSAGE_SIZE";

    // Cache the effective transport max frame size (derived from `NOVA_RPC_MAX_MESSAGE_SIZE`).
    //
    // This value is intended to be read once (on first use) in production.
    //
    // Tests sometimes need to validate different env var values within a single process. Using an
    // atomic cache (instead of `OnceLock`) lets us provide a test-only reset hook without affecting
    // production callers.
    static MAX_FRAME_SIZE: AtomicUsize = AtomicUsize::new(0);

    /// Test-only global lock for coordinating env var mutation and cache resets.
    ///
    /// This is `#[doc(hidden)]` and intentionally named to discourage production use.
    #[doc(hidden)]
    pub static __TRANSPORT_ENV_LOCK_FOR_TESTS: Mutex<()> = Mutex::new(());

    #[doc(hidden)]
    pub fn __reset_max_frame_size_cache_for_tests() {
        MAX_FRAME_SIZE.store(0, Ordering::Relaxed);
    }

    fn compute_max_frame_size() -> usize {
        static MAX_FRAME_SIZE_ENV_ERROR_LOGGED: OnceLock<()> = OnceLock::new();

        let raw = match std::env::var(MAX_MESSAGE_SIZE_ENV_VAR) {
            Ok(raw) => raw,
            Err(std::env::VarError::NotPresent) => {
                return DEFAULT_MAX_FRAME_BYTES.clamp(1, MAX_FRAME_BYTES)
            }
            Err(err) => {
                if MAX_FRAME_SIZE_ENV_ERROR_LOGGED.set(()).is_ok() {
                    tracing::debug!(
                        target = "nova.remote_proto",
                        key = MAX_MESSAGE_SIZE_ENV_VAR,
                        error = ?err,
                        "failed to read RPC max frame size env var; using default (best effort)"
                    );
                }
                return DEFAULT_MAX_FRAME_BYTES.clamp(1, MAX_FRAME_BYTES);
            }
        };

        let raw = raw.trim();
        if raw.is_empty() || raw == "0" {
            return DEFAULT_MAX_FRAME_BYTES.clamp(1, MAX_FRAME_BYTES);
        }

        let parsed = match raw.parse::<usize>() {
            Ok(value) => value,
            Err(err) => {
                if MAX_FRAME_SIZE_ENV_ERROR_LOGGED.set(()).is_ok() {
                    tracing::debug!(
                        target = "nova.remote_proto",
                        key = MAX_MESSAGE_SIZE_ENV_VAR,
                        raw = %raw,
                        error = %err,
                        "invalid RPC max frame size env var; using default (best effort)"
                    );
                }
                DEFAULT_MAX_FRAME_BYTES
            }
        };

        parsed.max(1).min(MAX_FRAME_BYTES)
    }

    fn max_frame_size() -> usize {
        let cached = MAX_FRAME_SIZE.load(Ordering::Relaxed);
        if cached != 0 {
            return cached;
        }

        let computed = compute_max_frame_size();
        // Races are benign here: multiple threads may compute and attempt to initialize the cache,
        // but they will all compute the same value (absent env var mutation, which is coordinated
        // in tests by `__TRANSPORT_ENV_LOCK_FOR_TESTS`).
        match MAX_FRAME_SIZE.compare_exchange(0, computed, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => computed,
            Err(existing) => existing,
        }
    }

    pub fn encode_frame(payload: &[u8]) -> anyhow::Result<Vec<u8>> {
        let max = max_frame_size();
        if payload.len() > max {
            return Err(anyhow!(
                "frame payload too large ({} bytes > max_frame_bytes={max})",
                payload.len()
            ));
        }

        let len: u32 = payload
            .len()
            .try_into()
            .map_err(|_| anyhow!("frame payload too large"))?;

        let mut out = Vec::with_capacity(LEN_PREFIX_BYTES + payload.len());
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(payload);
        Ok(out)
    }

    pub fn decode_frame(bytes: &[u8]) -> anyhow::Result<&[u8]> {
        if bytes.len() < LEN_PREFIX_BYTES {
            return Err(anyhow!(
                "truncated frame: need {LEN_PREFIX_BYTES} byte length prefix, got {} bytes",
                bytes.len()
            ));
        }

        let len = u32::from_le_bytes(bytes[0..LEN_PREFIX_BYTES].try_into().unwrap()) as usize;
        let max = max_frame_size();
        if len > max {
            return Err(anyhow!(
                "frame payload too large ({len} bytes > max_frame_bytes={max})"
            ));
        }

        let expected_len = LEN_PREFIX_BYTES
            .checked_add(len)
            .ok_or_else(|| anyhow!("frame length overflow"))?;

        match bytes.len().cmp(&expected_len) {
            std::cmp::Ordering::Less => Err(anyhow!(
                "truncated frame payload: expected {expected_len} bytes, got {} bytes",
                bytes.len()
            )),
            std::cmp::Ordering::Greater => Err(anyhow!(
                "trailing bytes after frame: expected {expected_len} bytes, got {} bytes",
                bytes.len()
            )),
            std::cmp::Ordering::Equal => Ok(&bytes[LEN_PREFIX_BYTES..expected_len]),
        }
    }

    pub fn encode_framed_message(msg: &RpcMessage) -> anyhow::Result<Vec<u8>> {
        let payload = encode_message(msg)?;
        encode_frame(&payload)
    }

    pub fn decode_framed_message(bytes: &[u8]) -> anyhow::Result<RpcMessage> {
        let payload = decode_frame(bytes)?;
        decode_message(payload)
    }

    #[cfg(feature = "tokio")]
    pub async fn write_payload(
        stream: &mut (impl tokio::io::AsyncWrite + Unpin),
        payload: &[u8],
    ) -> anyhow::Result<()> {
        use tokio::io::AsyncWriteExt;

        let max = max_frame_size();
        if payload.len() > max {
            return Err(anyhow!(
                "frame payload too large ({} bytes > max_frame_bytes={max})",
                payload.len()
            ));
        }
        let len: u32 = payload
            .len()
            .try_into()
            .map_err(|_| anyhow!("frame payload too large"))?;

        stream
            .write_all(&len.to_le_bytes())
            .await
            .context("write message len")?;
        stream
            .write_all(payload)
            .await
            .context("write message payload")?;
        stream.flush().await.context("flush message")?;
        Ok(())
    }

    #[cfg(feature = "tokio")]
    pub async fn read_payload(
        stream: &mut (impl tokio::io::AsyncRead + Unpin),
    ) -> anyhow::Result<Vec<u8>> {
        read_payload_limited(stream, max_frame_size()).await
    }

    /// Read a length-delimited payload, enforcing both the transport-level maximum and an
    /// additional caller-provided cap.
    ///
    /// This is useful for applying stricter limits during unauthenticated handshakes, while still
    /// allowing larger (but bounded) authenticated RPC frames.
    #[cfg(feature = "tokio")]
    pub async fn read_payload_limited(
        stream: &mut (impl tokio::io::AsyncRead + Unpin),
        max_len: usize,
    ) -> anyhow::Result<Vec<u8>> {
        use tokio::io::AsyncReadExt;

        let max_len = max_len.clamp(1, MAX_FRAME_BYTES);
        let max = max_frame_size().min(max_len);

        let mut prefix = [0u8; LEN_PREFIX_BYTES];
        stream
            .read_exact(&mut prefix)
            .await
            .context("read message len")?;
        let len = u32::from_le_bytes(prefix) as usize;
        if len > max {
            return Err(anyhow!(
                "frame payload too large ({len} bytes > max_frame_bytes={max})"
            ));
        }

        if len == 0 {
            return Ok(Vec::new());
        }

        // Grow the buffer gradually so a peer cannot force us to allocate `len` bytes up-front
        // and then stall (e.g. by sending only the length prefix). This keeps per-connection
        // memory bounded by the amount of payload actually received.
        let mut buf = Vec::new();
        buf.try_reserve_exact(len.min(8 * 1024))
            .context("allocate message buffer")?;

        while buf.len() < len {
            if buf.capacity() == buf.len() {
                let new_cap = (buf.capacity().saturating_mul(2)).min(len);
                buf.try_reserve_exact(new_cap.saturating_sub(buf.capacity()))
                    .context("allocate message buffer")?;
            }

            let remaining = len - buf.len();
            let spare = buf.capacity() - buf.len();
            let to_read = remaining.min(spare);

            let start = buf.len();
            buf.resize(start + to_read, 0);
            stream
                .read_exact(&mut buf[start..])
                .await
                .context("read message payload")?;
        }

        Ok(buf)
    }

    #[cfg(feature = "tokio")]
    pub async fn write_message(
        stream: &mut (impl tokio::io::AsyncWrite + Unpin),
        message: &RpcMessage,
    ) -> anyhow::Result<()> {
        let payload = encode_message(message)?;
        write_payload(stream, &payload).await
    }

    #[cfg(feature = "tokio")]
    pub async fn read_message(
        stream: &mut (impl tokio::io::AsyncRead + Unpin),
    ) -> anyhow::Result<RpcMessage> {
        read_message_limited(stream, max_frame_size()).await
    }

    #[cfg(feature = "tokio")]
    pub async fn read_message_limited(
        stream: &mut (impl tokio::io::AsyncRead + Unpin),
        max_len: usize,
    ) -> anyhow::Result<RpcMessage> {
        let payload = read_payload_limited(stream, max_len).await?;
        decode_message(&payload)
    }
}
