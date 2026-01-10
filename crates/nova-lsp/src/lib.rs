//! Nova Language Server Protocol integration.
//!
//! This crate is focused on exposing Nova-specific LSP extensions. Today that
//! includes:
//!
//! - Testing endpoints (backed by `nova-testing`)
//!   - `nova/test/discover`
//!   - `nova/test/run`
//! - Debugger-excellence endpoints
//!   - `nova/debug/configurations`
//!   - `nova/debug/hotSwap`

pub mod extensions;

mod server;

pub use server::{HotSwapParams, HotSwapService, NovaLspServer};

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

pub const DEBUG_CONFIGURATIONS_METHOD: &str = "nova/debug/configurations";
pub const DEBUG_HOT_SWAP_METHOD: &str = "nova/debug/hotSwap";

/// Dispatch a Nova custom request (`nova/*`) by method name.
///
/// This helper is designed to be embedded in whichever LSP transport is used
/// (e.g. `lsp-server`, `tower-lsp`). It only supports stateless endpoints.
pub fn handle_custom_request(
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value> {
    match method {
        TEST_DISCOVER_METHOD => extensions::test::handle_discover(params),
        TEST_RUN_METHOD => extensions::test::handle_run(params),
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
            let params: HotSwapParams = serde_json::from_value(params)
                .map_err(|err| NovaLspError::InvalidParams(err.to_string()))?;
            let hot_swap = hot_swap.ok_or_else(|| {
                NovaLspError::InvalidParams("hot-swap service is not available".into())
            })?;
            serde_json::to_value(server.hot_swap(hot_swap, params))
                .map_err(|err| NovaLspError::Internal(err.to_string()))
        }
        _ => handle_custom_request(method, params),
    }
}
