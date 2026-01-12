use std::time::Duration;

/// Maximum size of a fuzz input accepted by per-crate fuzz harnesses.
///
/// This matches the repository's other per-crate harness caps and prevents attacker-controlled
/// lengths from driving large allocations via `cargo fuzz ... -max_len ...`.
pub const MAX_INPUT_SIZE: usize = 256 * 1024; // 256 KiB

/// Wall-clock timeout per fuzz input.
pub const TIMEOUT: Duration = Duration::from_secs(1);

