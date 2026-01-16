use crate::{NovaLspError, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};

use crate::extensions::project::{source_root_kind_string, source_root_origin_string};

pub const SCHEMA_VERSION: u32 = 1;

pub fn handle_source_paths(params: serde_json::Value) -> Result<serde_json::Value> {
    let project_root = super::decode_project_root(params)?;
    if project_root.trim().is_empty() {
        return Err(NovaLspError::InvalidParams(
            "`projectRoot` must not be empty".to_string(),
        ));
    }

    let root = PathBuf::from(&project_root);
    let config = nova_project::load_project(&root)
        .map_err(|err| NovaLspError::Internal(format!("failed to load project: {err}")))?;

    let roots = config
        .source_roots
        .into_iter()
        .map(|root| {
            Value::Object({
                let mut value = serde_json::Map::new();
                value.insert(
                    "kind".to_string(),
                    Value::String(source_root_kind_string(root.kind).to_string()),
                );
                value.insert(
                    "origin".to_string(),
                    Value::String(source_root_origin_string(root.origin).to_string()),
                );
                value.insert(
                    "path".to_string(),
                    Value::String(root.path.to_string_lossy().to_string()),
                );
                value
            })
        })
        .collect::<Vec<_>>();

    Ok(Value::Object({
        let mut resp = serde_json::Map::new();
        resp.insert(
            "schemaVersion".to_string(),
            Value::from(u64::from(SCHEMA_VERSION)),
        );
        resp.insert("roots".to_string(), Value::Array(roots));
        resp
    }))
}

pub fn handle_resolve_main_class(params: serde_json::Value) -> Result<serde_json::Value> {
    let obj = params
        .as_object()
        .ok_or_else(|| NovaLspError::InvalidParams("params must be an object".to_string()))?;
    let project_root = super::get_str(obj, &["projectRoot", "project_root", "root"])
        .map(|s| s.to_string())
        .filter(|root| !root.trim().is_empty());
    let uri = obj
        .get("uri")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .filter(|uri| !uri.trim().is_empty());
    let include_tests = obj
        .get("includeTests")
        .or_else(|| obj.get("include_tests"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let classes = if let Some(root) = project_root.as_deref() {
        let project = nova_ide::Project::load_from_dir(Path::new(root))
            .map_err(|err| NovaLspError::Internal(err.to_string()))?;
        project.discover_classes()
    } else if let Some(uri) = uri.as_deref() {
        let url = url::Url::parse(uri).map_err(|err| {
            NovaLspError::InvalidParams(format!("`uri` must be a valid URI ({err})"))
        })?;
        let path = url
            .to_file_path()
            .map_err(|_| NovaLspError::InvalidParams("`uri` must be a file:// URI".to_string()))?;
        let text = std::fs::read_to_string(&path)
            .map_err(|err| NovaLspError::Internal(format!("failed to read {path:?}: {err}")))?;
        let project = nova_ide::Project::new(vec![(path, text)]);
        project.discover_classes()
    } else {
        return Err(NovaLspError::InvalidParams(
            "either `projectRoot` or `uri` must be provided".to_string(),
        ));
    };

    let mut classes = classes
        .into_iter()
        .filter(|class| {
            if !include_tests && class.is_test {
                return false;
            }
            class.has_main || class.is_spring_boot_app || (include_tests && class.is_test)
        })
        .map(|class| {
            Value::Object({
                let mut value = serde_json::Map::new();
                value.insert(
                    "qualifiedName".to_string(),
                    Value::String(class.qualified_name),
                );
                value.insert("simpleName".to_string(), Value::String(class.simple_name));
                value.insert(
                    "path".to_string(),
                    Value::String(class.path.to_string_lossy().to_string()),
                );
                value.insert("hasMain".to_string(), Value::Bool(class.has_main));
                value.insert("isTest".to_string(), Value::Bool(class.is_test));
                value.insert(
                    "isSpringBootApp".to_string(),
                    Value::Bool(class.is_spring_boot_app),
                );
                value
            })
        })
        .collect::<Vec<_>>();

    classes.sort_by(|a, b| {
        let a_name = a
            .get("qualifiedName")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let b_name = b
            .get("qualifiedName")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        a_name.cmp(b_name).then_with(|| {
            let a_path = a.get("path").and_then(|v| v.as_str()).unwrap_or_default();
            let b_path = b.get("path").and_then(|v| v.as_str()).unwrap_or_default();
            a_path.cmp(b_path)
        })
    });

    Ok(Value::Object({
        let mut resp = serde_json::Map::new();
        resp.insert(
            "schemaVersion".to_string(),
            Value::from(u64::from(SCHEMA_VERSION)),
        );
        resp.insert("classes".to_string(), Value::Array(classes));
        resp
    }))
}
