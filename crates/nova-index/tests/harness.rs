//! Integration test harness for `nova-index`.
//!
//! This crate exists so all integration tests in `crates/nova-index/tests/` are
//! compiled into a single test binary (faster `cargo test` / less duplicated
//! compilation work).

mod suite;

