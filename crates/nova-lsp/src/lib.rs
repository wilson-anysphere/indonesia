//! Nova Language Server Protocol integration.
//!
//! This crate is focused on exposing Nova-specific LSP extensions. Today that
//! includes:
//!
//! - Testing endpoints (backed by `nova-testing`)
//!   - `nova/test/discover`
//!   - `nova/test/run`
//! - Build integration endpoints (backed by `nova-build`)
//!   - `nova/buildProject`
//!   - `nova/java/classpath`
//!   - `nova/reloadProject`
//! - Annotation processing endpoints (backed by `nova-apt`)
//!   - `nova/java/generatedSources`
//!   - `nova/java/runAnnotationProcessing`
//! - Debugger-excellence endpoints
//!   - `nova/debug/configurations`
//!   - `nova/debug/hotSwap`
//! - AI augmentation endpoints (implemented in the `nova-lsp` binary)
//!   - `nova/ai/explainError`
//!   - `nova/ai/generateMethodBody`
//!   - `nova/ai/generateTests`

pub mod extensions;
pub mod extract_method;
pub mod refactor;

mod distributed;
mod server;
pub mod workspace_edit;

pub use distributed::NovaLspFrontend;
pub use refactor::{safe_delete_code_action, RefactorResponse};
pub use server::{HotSwapParams, HotSwapService, NovaLspServer};
pub use workspace_edit::{client_supports_file_operations, workspace_edit_from_refactor};

use nova_dap::hot_swap::{BuildSystem, JdwpRedefiner};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum NovaLspError {
    #[error("invalid params: {0}")]
    InvalidParams(String),
    #[error("internal error: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, NovaLspError>;

pub const TEST_DISCOVER_METHOD: &str = "nova/test/discover";
pub const TEST_RUN_METHOD: &str = "nova/test/run";
pub const BUILD_PROJECT_METHOD: &str = "nova/buildProject";
pub const JAVA_CLASSPATH_METHOD: &str = "nova/java/classpath";
pub const JAVA_GENERATED_SOURCES_METHOD: &str = "nova/java/generatedSources";
pub const RUN_ANNOTATION_PROCESSING_METHOD: &str = "nova/java/runAnnotationProcessing";
pub const RELOAD_PROJECT_METHOD: &str = "nova/reloadProject";

pub const DEBUG_CONFIGURATIONS_METHOD: &str = "nova/debug/configurations";
pub const DEBUG_HOT_SWAP_METHOD: &str = "nova/debug/hotSwap";

// AI custom requests (handled by the `nova-lsp` binary).
pub const AI_EXPLAIN_ERROR_METHOD: &str = "nova/ai/explainError";
pub const AI_GENERATE_METHOD_BODY_METHOD: &str = "nova/ai/generateMethodBody";
pub const AI_GENERATE_TESTS_METHOD: &str = "nova/ai/generateTests";

/// Dispatch a Nova custom request (`nova/*`) by method name.
///
/// This helper is designed to be embedded in whichever LSP transport is used
/// (e.g. `lsp-server`, `tower-lsp`). It only supports stateless endpoints.
pub fn handle_custom_request(method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
    match method {
        TEST_DISCOVER_METHOD => extensions::test::handle_discover(params),
        TEST_RUN_METHOD => extensions::test::handle_run(params),
        BUILD_PROJECT_METHOD => extensions::build::handle_build_project(params),
        JAVA_CLASSPATH_METHOD => extensions::build::handle_java_classpath(params),
        JAVA_GENERATED_SOURCES_METHOD => extensions::apt::handle_generated_sources(params),
        RUN_ANNOTATION_PROCESSING_METHOD => extensions::apt::handle_run_annotation_processing(params),
        RELOAD_PROJECT_METHOD => extensions::build::handle_reload_project(params),
        DEBUG_CONFIGURATIONS_METHOD => extensions::debug::handle_debug_configurations(params),
        DEBUG_HOT_SWAP_METHOD => extensions::debug::handle_hot_swap(params),
        _ => Err(NovaLspError::InvalidParams(format!(
            "unknown (stateless) method: {method}"
        ))),
    }
}

/// Dispatch a Nova custom request (`nova/*`) with access to the loaded project.
///
/// This is the path used by the debugging extensions, which need project
/// context (and, for hot swapping, access to the active debug session).
pub fn handle_custom_request_with_state<B: BuildSystem, J: JdwpRedefiner>(
    server: &NovaLspServer,
    hot_swap: Option<&mut HotSwapService<B, J>>,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value> {
    match method {
        DEBUG_CONFIGURATIONS_METHOD => serde_json::to_value(server.debug_configurations())
            .map_err(|err| NovaLspError::Internal(err.to_string())),
        DEBUG_HOT_SWAP_METHOD => {
            let params: HotSwapParams =
                serde_json::from_value(params).map_err(|err| NovaLspError::InvalidParams(err.to_string()))?;
            let hot_swap = hot_swap.ok_or_else(|| {
                NovaLspError::InvalidParams("hot-swap service is not available".into())
            })?;
            serde_json::to_value(server.hot_swap(hot_swap, params))
                .map_err(|err| NovaLspError::Internal(err.to_string()))
        }
        _ => handle_custom_request(method, params),
    }
}

// -----------------------------------------------------------------------------
// Core LSP request delegation
// -----------------------------------------------------------------------------

/// Delegate completion requests to `nova-ide`.
pub fn completion(
    db: &dyn nova_db::Database,
    file: nova_db::FileId,
    position: lsp_types::Position,
) -> Vec<lsp_types::CompletionItem> {
    nova_ide::completions(db, file, position)
}

/// Delegate hover requests to `nova-ide`.
pub fn hover(
    db: &dyn nova_db::Database,
    file: nova_db::FileId,
    position: lsp_types::Position,
) -> Option<lsp_types::Hover> {
    nova_ide::hover(db, file, position)
}

/// Delegate go-to-definition requests to `nova-ide`.
pub fn goto_definition(
    db: &dyn nova_db::Database,
    file: nova_db::FileId,
    position: lsp_types::Position,
) -> Option<lsp_types::Location> {
    nova_ide::goto_definition(db, file, position)
}

/// Delegate diagnostics to `nova-ide`.
pub fn diagnostics(
    db: &dyn nova_db::Database,
    file: nova_db::FileId,
) -> Vec<lsp_types::Diagnostic> {
    nova_ide::file_diagnostics_lsp(db, file)
}
