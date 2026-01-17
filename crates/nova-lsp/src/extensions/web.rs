use std::path::Path;

use crate::{NovaLspError, Result};
use serde_json::{Map, Value};

pub fn handle_endpoints(params: serde_json::Value) -> Result<serde_json::Value> {
    let project_root = super::decode_project_root(params)?;
    if project_root.trim().is_empty() {
        return Err(NovaLspError::InvalidParams(
            "`projectRoot` must not be empty".to_string(),
        ));
    }

    let endpoints = nova_framework_web::extract_http_endpoints_in_dir(Path::new(&project_root))
        .map_err(|err| NovaLspError::InvalidParams(err.to_string()))?;

    let endpoints = Value::Array(
        endpoints
            .into_iter()
            .map(|ep| {
                let mut obj = Map::new();
                obj.insert("path".to_string(), Value::String(ep.path));
                obj.insert(
                    "methods".to_string(),
                    Value::Array(ep.methods.into_iter().map(Value::String).collect()),
                );
                obj.insert(
                    "file".to_string(),
                    ep.handler
                        .file
                        .and_then(|p| p.to_str().map(|s| s.to_string()))
                        .map_or(Value::Null, Value::String),
                );
                obj.insert("line".to_string(), Value::from(ep.handler.line));
                Value::Object(obj)
            })
            .collect(),
    );

    let mut resp = Map::new();
    resp.insert("endpoints".to_string(), endpoints);
    Ok(Value::Object(resp))
}
