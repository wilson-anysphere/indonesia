use std::time::Duration;

/// Maximum size of a fuzz input accepted by per-crate fuzz harnesses.
///
/// This matches the root `fuzz/` harness cap and prevents `cargo fuzz ... -max_len ...` from
/// driving huge allocations or quadratic behavior via attacker-controlled lengths.
pub const MAX_INPUT_SIZE: usize = 256 * 1024; // 256 KiB

/// Wall-clock timeout per fuzz input.
pub const TIMEOUT: Duration = Duration::from_secs(1);
