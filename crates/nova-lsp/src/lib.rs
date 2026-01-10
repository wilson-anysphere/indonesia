//! Nova Language Server Protocol integration.
//!
//! This crate is currently focused on exposing Nova-specific LSP extensions. The request/response
//! payloads are defined in `nova-testing` for the testing endpoints.

pub mod extensions;

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

/// Dispatch a Nova custom request (`nova/*`) by method name.
///
/// This helper is designed to be embedded in whichever LSP transport is used
/// (e.g. `lsp-server`, `tower-lsp`).
pub fn handle_custom_request(method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
    match method {
        TEST_DISCOVER_METHOD => extensions::test::handle_discover(params),
        TEST_RUN_METHOD => extensions::test::handle_run(params),
        _ => Err(NovaLspError::InvalidParams(format!(
            "unknown method: {method}"
        ))),
    }
}
