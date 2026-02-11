use crate::{NovaLspError, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::extensions::project::SourceRootEntry;

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JavaSourcePathsParams {
    /// Workspace root on disk.
    ///
    /// Clients should prefer `projectRoot` (camelCase). `root` is accepted as an
    /// alias for parity with other Nova extension endpoints.
    #[serde(alias = "root")]
    pub project_root: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JavaSourcePathsResponse {
    pub schema_version: u32,
    pub roots: Vec<SourceRootEntry>,
}

pub fn handle_source_paths(params: serde_json::Value) -> Result<serde_json::Value> {
    let params: JavaSourcePathsParams = serde_json::from_value(params)
        .map_err(|err| NovaLspError::InvalidParams(crate::sanitize_serde_json_error(&err)))?;

    if params.project_root.trim().is_empty() {
        return Err(NovaLspError::InvalidParams(
            "`projectRoot` must not be empty".to_string(),
        ));
    }

    let root = PathBuf::from(&params.project_root);
    let config = nova_project::load_project(&root)
        .map_err(|err| NovaLspError::Internal(format!("failed to load project: {err}")))?;

    let roots = config
        .source_roots
        .into_iter()
        .map(|root| SourceRootEntry {
            kind: root.kind.into(),
            origin: root.origin.into(),
            path: root.path.to_string_lossy().to_string(),
        })
        .collect();

    let resp = JavaSourcePathsResponse {
        schema_version: SCHEMA_VERSION,
        roots,
    };
    serde_json::to_value(resp)
        .map_err(|err| NovaLspError::Internal(crate::sanitize_serde_json_error(&err)))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolveMainClassParams {
    #[serde(default, alias = "root")]
    pub project_root: Option<String>,
    #[serde(default)]
    pub uri: Option<String>,
    #[serde(default)]
    pub include_tests: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedJavaClass {
    pub qualified_name: String,
    pub simple_name: String,
    pub path: String,
    pub has_main: bool,
    pub is_test: bool,
    pub is_spring_boot_app: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolveMainClassResponse {
    pub schema_version: u32,
    pub classes: Vec<ResolvedJavaClass>,
}

pub fn handle_resolve_main_class(params: serde_json::Value) -> Result<serde_json::Value> {
    let params: ResolveMainClassParams = serde_json::from_value(params)
        .map_err(|err| NovaLspError::InvalidParams(crate::sanitize_serde_json_error(&err)))?;

    let classes = if let Some(root) = params
        .project_root
        .as_deref()
        .filter(|root| !root.trim().is_empty())
    {
        let project = nova_ide::Project::load_from_dir(Path::new(root))
            .map_err(|err| NovaLspError::Internal(err.to_string()))?;
        project.discover_classes()
    } else if let Some(uri) = params.uri.as_deref().filter(|uri| !uri.trim().is_empty()) {
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

    let mut classes: Vec<ResolvedJavaClass> = classes
        .into_iter()
        .filter(|class| {
            if !params.include_tests && class.is_test {
                return false;
            }
            class.has_main || class.is_spring_boot_app || (params.include_tests && class.is_test)
        })
        .map(|class| ResolvedJavaClass {
            qualified_name: class.qualified_name,
            simple_name: class.simple_name,
            path: class.path.to_string_lossy().to_string(),
            has_main: class.has_main,
            is_test: class.is_test,
            is_spring_boot_app: class.is_spring_boot_app,
        })
        .collect();

    classes.sort_by(|a, b| {
        a.qualified_name
            .cmp(&b.qualified_name)
            .then(a.path.cmp(&b.path))
    });

    let resp = ResolveMainClassResponse {
        schema_version: SCHEMA_VERSION,
        classes,
    };

    serde_json::to_value(resp)
        .map_err(|err| NovaLspError::Internal(crate::sanitize_serde_json_error(&err)))
}
