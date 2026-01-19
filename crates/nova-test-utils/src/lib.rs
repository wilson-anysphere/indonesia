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

#[cfg(feature = "fixtures")]
mod fixtures;

#[cfg(feature = "fixtures")]
pub use fixtures::*;

#[cfg(feature = "javac")]
pub mod javac;
