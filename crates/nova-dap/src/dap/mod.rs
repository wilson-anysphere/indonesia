pub mod codec;
pub mod messages;
pub mod types;

/// Maximum allowed DAP message payload size (in bytes).
///
/// This caps the value of the incoming `Content-Length` header. Without an upper bound, a
/// malformed/hostile client can send an enormous `Content-Length` and force the adapter to
/// allocate huge buffers (potentially triggering OOM / RLIMIT_AS kills) before we even attempt to
/// read the message body.
///
/// 16 MiB is intentionally generous for typical DAP JSON messages, while still bounding worst-case
/// allocations. We also support DAP flows that embed base64 blobs (e.g. hot-swap class bytes), so
/// this should not be too small; adjust upward if we ever legitimately exceed it.
pub const MAX_DAP_MESSAGE_BYTES: usize = 16 * 1024 * 1024; // 16 MiB

/// Maximum allowed size of a single DAP header line (in bytes).
pub const MAX_DAP_HEADER_LINE_BYTES: usize = 8 * 1024; // 8 KiB
