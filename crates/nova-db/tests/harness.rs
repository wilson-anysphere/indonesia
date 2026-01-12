//! Integration test harness for `nova-db`.
//!
//! The `tests/` directory previously contained multiple root `*.rs` files, each compiled as its
//! own integration test crate. Consolidating them into a single crate drastically reduces
//! compile times for `cargo test -p nova-db --tests`.

mod suite;
