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
//!
//! When adding new integration tests, put them under `tests/suite/` and register them from
//! `tests/suite/mod.rs`. Avoid adding new root-level `tests/*.rs` files: each file becomes its own
//! integration test binary, which is expensive under the agent memory constraints and is enforced
//! by repo invariants.
//!
//! Note: older instructions may refer to `cargo test --locked -p nova-db --test typeck`. We keep a
//! small dedicated `typeck` integration test target (see `[[test]] name = "typeck"` in
//! `crates/nova-db/Cargo.toml`) for compatibility, but prefer running tests via this consolidated
//! harness for compile-time and memory efficiency.

mod suite;
