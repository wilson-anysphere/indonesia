//! Bazel build integration for Nova.
//!
//! This crate focuses on extracting enough information from Bazel to power Java semantic
//! analysis in a language server:
//! - workspace discovery (`WORKSPACE`, `WORKSPACE.bazel`, `MODULE.bazel`)
//! - Java target discovery via `bazel query`
//! - per-target classpath / module-path / source roots via `bazel aquery` (Javac actions)
//! - mapping workspace source files to owning `java_*` targets (for hot swap)
//! - on-demand file -> compile-info resolution (find owners + load `JavaCompileInfo` for the best/first owner)
//! - caching keyed by query/aquery expression version and Bazel build definition/config file digests

mod aquery;
mod build;
mod cache;
mod command;
mod workspace;

// The BSP module is optional at runtime, but we still compile it for unit tests so
// the protocol glue (JSON deserialization, diagnostics mapping) remains covered.
#[cfg(any(test, feature = "bsp"))]
pub mod bsp;

#[cfg(any(test, feature = "bsp"))]
mod bsp_config;

#[cfg(any(test, feature = "bsp"))]
mod orchestrator;

#[cfg(feature = "bsp")]
pub use crate::bsp::{BspClient, BspCompileOutcome, BspServerConfig, BspWorkspace};

pub use crate::{
    aquery::{
        extract_java_compile_info, parse_aquery_textproto, parse_aquery_textproto_streaming,
        JavaCompileInfo, JavacAction,
    },
    build::BazelBuildOptions,
    cache::{
        digest_file, digest_file_or_absent, BazelCache, CacheEntry, CompileInfoProvider, FileDigest,
    },
    command::{CommandOutput, CommandRunner, DefaultCommandRunner},
    workspace::{
        bazel_workspace_root, is_bazel_workspace, BazelWorkspace, BazelWorkspaceDiscovery,
    },
};

#[cfg(any(test, feature = "bsp"))]
pub use crate::bsp::{
    bsp_compile_and_collect_diagnostics, bsp_publish_diagnostics_to_nova_diagnostics,
    BazelBspConfig,
};

#[cfg(any(test, feature = "bsp"))]
pub use crate::orchestrator::{
    BazelBuildDiagnosticsSnapshot, BazelBuildExecutor, BazelBuildOrchestrator, BazelBuildRequest,
    BazelBuildStatusSnapshot, BazelBuildTaskId, BazelBuildTaskState, DefaultBazelBuildExecutor,
};

#[cfg(feature = "bsp")]
pub use crate::bsp::target_compile_info_via_bsp;

/// Test-only helpers.
///
/// This is `pub` so it can be used from integration tests (which compile `nova-build-bazel` as a
/// normal dependency, without `cfg(test)`).
#[cfg(any(test, feature = "bsp"))]
#[doc(hidden)]
pub mod test_support {
    use std::sync::{Mutex, MutexGuard, OnceLock};

    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    pub fn env_lock() -> MutexGuard<'static, ()> {
        ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }
}
