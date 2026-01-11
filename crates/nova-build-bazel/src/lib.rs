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

// The BSP module is optional at runtime, but we still compile it for unit tests so
// the protocol glue (JSON deserialization, diagnostics mapping) remains covered.
#[cfg(any(test, feature = "bsp"))]
pub mod bsp;

pub use crate::{
    aquery::{extract_java_compile_info, parse_aquery_textproto, JavaCompileInfo, JavacAction},
    cache::{digest_file, BazelCache, BuildFileDigest, CacheEntry},
    command::{CommandOutput, CommandRunner, DefaultCommandRunner},
    workspace::{BazelWorkspace, BazelWorkspaceDiscovery},
};

#[cfg(any(test, feature = "bsp"))]
pub use crate::bsp::{
    bsp_compile_and_collect_diagnostics, bsp_publish_diagnostics_to_nova_diagnostics,
    BazelBspConfig,
};
