use std::path::Path;

use crate::{NovaLspError, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebEndpointsRequest {
    pub project_root: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebEndpoint {
    pub path: String,
    pub methods: Vec<String>,
    pub file: Option<String>,
    /// 1-based line number.
    pub line: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebEndpointsResponse {
    pub endpoints: Vec<WebEndpoint>,
}

pub fn handle_endpoints(params: serde_json::Value) -> Result<serde_json::Value> {
    let req: WebEndpointsRequest = serde_json::from_value(params)
        .map_err(|err| NovaLspError::InvalidParams(crate::sanitize_serde_json_error(&err)))?;

    let endpoints = nova_framework_web::extract_http_endpoints_in_dir(Path::new(&req.project_root))
        .map_err(|err| NovaLspError::InvalidParams(err.to_string()))?;

    let resp = WebEndpointsResponse {
        endpoints: endpoints
            .into_iter()
            .map(|ep| WebEndpoint {
                path: ep.path,
                methods: ep.methods,
                file: ep
                    .handler
                    .file
                    .and_then(|p| p.to_str().map(|s| s.to_string())),
                line: ep.handler.line,
            })
            .collect(),
    };

    serde_json::to_value(resp).map_err(|err| NovaLspError::Internal(err.to_string()))
}
