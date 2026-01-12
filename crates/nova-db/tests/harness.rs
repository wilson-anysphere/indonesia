//! Integration test harness for `nova-db`.
//!
//! The `tests/` directory previously contained multiple root `*.rs` files, each compiled as its
//! own integration test crate. Consolidating them into a single crate drastically reduces
//! compile times for `cargo test --locked -p nova-db --tests`.
//!
//! To run only a subset of tests (for example, the type-checking suite), use a scoped filter:
//! ```bash
//! bash scripts/cargo_agent.sh test --locked -p nova-db --test harness suite::typeck
//! ```

mod suite;
