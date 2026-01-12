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
//! ```bash
//! # Run ignored tests (requires `javac` on PATH)
//! cargo test -p nova-types --test integration javac_differential -- --ignored
//! ```

pub mod env;

pub use env::{env_lock, EnvVarGuard};

#[cfg(feature = "fixtures")]
mod fixtures;

#[cfg(feature = "fixtures")]
pub use fixtures::*;

pub mod javac;
