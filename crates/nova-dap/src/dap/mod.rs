pub mod codec;
pub mod messages;
pub mod types;

/// Maximum size of a single DAP message payload (the JSON bytes, not including headers).
///
/// DAP messages are framed by a `Content-Length` header. Without an explicit upper bound, a
/// malicious peer can advertise an arbitrarily large length and trigger an outsized allocation.
pub const MAX_DAP_MESSAGE_BYTES: usize = 16 * 1024 * 1024; // 16 MiB
