//! Utilities shared by Nova tests.
//!
//! This crate contains small helpers for fixture-based tests across the
//! workspace (refactorings, LSP navigation, etc).
//!
//! It also includes a small differential harness against `javac` (see
//! [`javac`](crate::javac)).
//!
//! ## Running `javac` differential tests locally
//!
//! Use the agent cargo wrapper (it enforces `RLIMIT_AS` and build slot throttling on shared hosts):
//!
//! ```bash
//! # Run ignored tests (requires `javac` on PATH)
//! bash scripts/cargo_agent.sh test --locked -p nova-types --test javac_differential -- --ignored
//! ```

pub mod env;

pub use env::{env_lock, EnvVarGuard};

#[cfg(feature = "fixture-db")]
mod fixture_db;

#[cfg(feature = "fixture-fs")]
mod fixture_fs;

#[cfg(feature = "fixture-ranges")]
mod fixture_ranges;

#[cfg(feature = "lsp-text")]
mod lsp_text;

#[cfg(feature = "fixture-db")]
pub use fixture_db::*;

#[cfg(feature = "fixture-fs")]
pub use fixture_fs::*;

#[cfg(feature = "fixture-ranges")]
pub use fixture_ranges::*;

#[cfg(feature = "lsp-text")]
pub use lsp_text::*;

#[cfg(feature = "javac")]
pub mod javac;
