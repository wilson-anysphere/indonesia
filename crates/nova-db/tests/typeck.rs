//! Standalone type-checking integration test binary for `nova-db`.
//!
//! The main `tests/harness.rs` consolidates most integration tests into a single crate to reduce
//! compile time for `cargo test --locked -p nova-db --tests`. This wrapper exists so CI and developers can
//! run only the type-checking suite via:
//!
//!   cargo test --locked -p nova-db --test typeck

// Core typeck regression tests live in `tests/suite/typeck.rs`.
#[path = "suite/typeck.rs"]
mod suite_typeck;
