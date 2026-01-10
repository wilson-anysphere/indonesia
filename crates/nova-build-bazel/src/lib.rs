//! Bazel build integration for Nova.
//!
//! This crate focuses on extracting enough information from Bazel to power Java semantic
//! analysis in a language server:
//! - workspace discovery (`WORKSPACE`, `WORKSPACE.bazel`, `MODULE.bazel`)
//! - Java target discovery via `bazel query`
//! - per-target classpath / module-path / source roots via `bazel aquery` (Javac actions)
//! - caching keyed by query hash and BUILD file digests

mod aquery;
mod cache;
mod command;
mod workspace;

#[cfg(feature = "bsp")]
pub mod bsp;

pub use crate::{
    aquery::{extract_java_compile_info, parse_aquery_textproto, JavaCompileInfo, JavacAction},
    cache::{digest_file, BazelCache, BuildFileDigest, CacheEntry},
    command::{CommandOutput, CommandRunner, DefaultCommandRunner},
    workspace::{BazelWorkspace, BazelWorkspaceDiscovery},
};
