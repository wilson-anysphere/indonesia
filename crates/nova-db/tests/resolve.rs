//! Name resolution + import diagnostics integration test entry point for `nova-db`.
//!
//! `nova-db` consolidates most integration tests into `tests/harness.rs` for compile-time
//! performance. This crate exists solely to provide a stable, narrowly-scoped target for:
//! `cargo test -p nova-db --test resolve`.
//!
//! The actual tests live in `tests/suite/resolve.rs`.

#[path = "suite/resolve.rs"]
mod suite_resolve;

