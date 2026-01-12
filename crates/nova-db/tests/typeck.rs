//! Type-checking integration test entry point for `nova-db`.
//!
//! `nova-db` consolidates most integration tests into `tests/harness.rs` for compile-time
//! performance. This crate exists solely to provide a stable, narrowly-scoped target for:
//! `cargo test -p nova-db --test typeck`.

// Core typeck regression tests live in `tests/suite/typeck.rs`.
#[path = "suite/typeck.rs"]
mod suite_typeck;
