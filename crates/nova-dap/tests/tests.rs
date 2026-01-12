// Consolidated integration test harness.
//
// Each `tests/*.rs` file becomes a separate Cargo integration test binary. Under
// the `cargo_agent` RLIMIT_AS constraints this is expensive, so `nova-dap`
// intentionally uses a single harness file that `mod`s the rest of the suite.
mod harness;
mod suite;
