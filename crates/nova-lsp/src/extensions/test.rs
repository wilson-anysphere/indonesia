use crate::{NovaLspError, Result};
use nova_testing::schema::{TestDebugRequest, TestDiscoverRequest, TestRunRequest};

pub fn handle_discover(params: serde_json::Value) -> Result<serde_json::Value> {
    let req: TestDiscoverRequest = serde_json::from_value(params)
        .map_err(|err| NovaLspError::InvalidParams(err.to_string()))?;
    let resp = nova_testing::discover_tests(&req).map_err(map_testing_error)?;
    serde_json::to_value(resp).map_err(|err| NovaLspError::Internal(err.to_string()))
}

pub fn handle_run(params: serde_json::Value) -> Result<serde_json::Value> {
    let req: TestRunRequest = serde_json::from_value(params)
        .map_err(|err| NovaLspError::InvalidParams(err.to_string()))?;
    let resp = nova_testing::run_tests(&req).map_err(map_testing_error)?;
    serde_json::to_value(resp).map_err(|err| NovaLspError::Internal(err.to_string()))
}

pub fn handle_debug_configuration(params: serde_json::Value) -> Result<serde_json::Value> {
    let req: TestDebugRequest = serde_json::from_value(params)
        .map_err(|err| NovaLspError::InvalidParams(err.to_string()))?;
    let resp = nova_testing::debug::debug_configuration_for_request(&req).map_err(map_testing_error)?;
    serde_json::to_value(resp).map_err(|err| NovaLspError::Internal(err.to_string()))
}

fn map_testing_error(err: nova_testing::NovaTestingError) -> NovaLspError {
    match err {
        nova_testing::NovaTestingError::InvalidRequest(msg) => NovaLspError::InvalidParams(msg),
        other => NovaLspError::Internal(other.to_string()),
    }
}
