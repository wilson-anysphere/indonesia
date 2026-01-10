use crate::{NovaLspError, Result};
use nova_build::{BuildError, BuildManager, BuildResult, Classpath};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Parameters accepted by Nova's build-related extension requests.
///
/// This is intentionally loose; clients can omit `buildTool` to rely on
/// auto-detection.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NovaProjectParams {
    /// Workspace root on disk.
    ///
    /// Clients should prefer `projectRoot` (camelCase). `root` is accepted for
    /// backwards compatibility with early experiments.
    #[serde(alias = "root")]
    pub project_root: String,

    /// Explicit build tool selection.
    ///
    /// Clients should prefer `buildTool`. `kind` is accepted as an alias.
    #[serde(default, alias = "kind")]
    pub build_tool: Option<BuildTool>,

    /// For Maven projects, a path relative to `projectRoot` identifying the module.
    #[serde(default)]
    pub module: Option<String>,
    /// For Gradle projects, a Gradle project path (e.g. `:app`).
    #[serde(default, alias = "project_path")]
    pub project_path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BuildTool {
    Auto,
    Maven,
    Gradle,
}

#[derive(Debug, Serialize)]
pub struct NovaClasspathResponse {
    pub classpath: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct NovaBuildProjectResponse {
    pub diagnostics: Vec<NovaDiagnostic>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NovaPosition {
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NovaRange {
    pub start: NovaPosition,
    pub end: NovaPosition,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum NovaDiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NovaDiagnostic {
    pub file: String,
    pub range: NovaRange,
    pub severity: NovaDiagnosticSeverity,
    pub message: String,
    pub source: Option<String>,
}

impl From<nova_core::Diagnostic> for NovaDiagnostic {
    fn from(value: nova_core::Diagnostic) -> Self {
        Self {
            file: value.file.to_string_lossy().to_string(),
            range: NovaRange {
                start: NovaPosition {
                    line: value.range.start.line,
                    character: value.range.start.character,
                },
                end: NovaPosition {
                    line: value.range.end.line,
                    character: value.range.end.character,
                },
            },
            severity: match value.severity {
                nova_core::DiagnosticSeverity::Error => NovaDiagnosticSeverity::Error,
                nova_core::DiagnosticSeverity::Warning => NovaDiagnosticSeverity::Warning,
                nova_core::DiagnosticSeverity::Information => NovaDiagnosticSeverity::Information,
                nova_core::DiagnosticSeverity::Hint => NovaDiagnosticSeverity::Hint,
            },
            message: value.message,
            source: value.source,
        }
    }
}

pub fn handle_build_project(params: serde_json::Value) -> Result<serde_json::Value> {
    let params = parse_params(params)?;
    let manager = build_manager(&params);
    let result = run_build(&manager, &params)?;
    let resp = NovaBuildProjectResponse {
        diagnostics: result
            .diagnostics
            .into_iter()
            .map(NovaDiagnostic::from)
            .collect(),
    };
    serde_json::to_value(resp).map_err(|err| NovaLspError::Internal(err.to_string()))
}

pub fn handle_java_classpath(params: serde_json::Value) -> Result<serde_json::Value> {
    let params = parse_params(params)?;
    let manager = build_manager(&params);
    let cp = run_classpath(&manager, &params)?;
    let resp = NovaClasspathResponse {
        classpath: cp
            .entries
            .into_iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect(),
    };
    serde_json::to_value(resp).map_err(|err| NovaLspError::Internal(err.to_string()))
}

pub fn handle_reload_project(params: serde_json::Value) -> Result<serde_json::Value> {
    let params = parse_params(params)?;
    let manager = build_manager(&params);
    manager
        .reload_project(Path::new(&params.project_root))
        .map_err(map_build_error)?;
    Ok(serde_json::Value::Null)
}

fn parse_params(value: serde_json::Value) -> Result<NovaProjectParams> {
    serde_json::from_value(value).map_err(|err| NovaLspError::InvalidParams(err.to_string()))
}

fn build_manager(params: &NovaProjectParams) -> BuildManager {
    let root = PathBuf::from(&params.project_root);
    let cache_dir = root.join(".nova").join("build-cache");
    BuildManager::new(cache_dir)
}

fn run_build(build: &BuildManager, params: &NovaProjectParams) -> Result<BuildResult> {
    let root = PathBuf::from(&params.project_root);
    match detect_kind(&root, params.build_tool)? {
        BuildKind::Maven => build
            .build_maven(&root, params.module.as_deref().map(Path::new))
            .map_err(map_build_error),
        BuildKind::Gradle => build
            .build_gradle(&root, params.project_path.as_deref())
            .map_err(map_build_error),
    }
}

fn run_classpath(build: &BuildManager, params: &NovaProjectParams) -> Result<Classpath> {
    let root = PathBuf::from(&params.project_root);
    match detect_kind(&root, params.build_tool)? {
        BuildKind::Maven => build
            .classpath_maven(&root, params.module.as_deref().map(Path::new))
            .map_err(map_build_error),
        BuildKind::Gradle => build
            .classpath_gradle(&root, params.project_path.as_deref())
            .map_err(map_build_error),
    }
}

enum BuildKind {
    Maven,
    Gradle,
}

fn detect_kind(root: &Path, explicit: Option<BuildTool>) -> Result<BuildKind> {
    if let Some(tool) = explicit {
        return match tool {
            BuildTool::Maven => Ok(BuildKind::Maven),
            BuildTool::Gradle => Ok(BuildKind::Gradle),
            BuildTool::Auto => auto_detect_kind(root),
        };
    }

    auto_detect_kind(root)
}

fn map_build_error(err: BuildError) -> NovaLspError {
    NovaLspError::Internal(err.to_string())
}

fn auto_detect_kind(root: &Path) -> Result<BuildKind> {
    if root.join("pom.xml").exists() {
        return Ok(BuildKind::Maven);
    }
    if root.join("settings.gradle").exists()
        || root.join("settings.gradle.kts").exists()
        || root.join("build.gradle").exists()
        || root.join("build.gradle.kts").exists()
    {
        return Ok(BuildKind::Gradle);
    }

    Err(NovaLspError::InvalidParams(format!(
        "unsupported project root {}",
        root.display()
    )))
}

// -----------------------------------------------------------------------------
// Target-aware build metadata (Bazel/BSP)
// -----------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetClasspathParams {
    #[serde(alias = "root")]
    pub project_root: String,
    #[serde(default)]
    pub target: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetClasspathResult {
    pub project_root: String,
    #[serde(default)]
    pub target: Option<String>,
    pub classpath: Vec<String>,
    #[serde(default)]
    pub module_path: Vec<String>,
    #[serde(default)]
    pub source_roots: Vec<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub target_version: Option<String>,
}

pub fn handle_target_classpath(params: serde_json::Value) -> Result<serde_json::Value> {
    let req: TargetClasspathParams = serde_json::from_value(params)
        .map_err(|err| NovaLspError::InvalidParams(err.to_string()))?;

    if req.project_root.trim().is_empty() {
        return Err(NovaLspError::InvalidParams(
            "`projectRoot` must not be empty".to_string(),
        ));
    }

    let requested_root = PathBuf::from(&req.project_root);
    let requested_root = requested_root
        .canonicalize()
        .unwrap_or_else(|_| requested_root.clone());

    if let Some(workspace_root) = nova_project::bazel_workspace_root(&requested_root) {
        let Some(target) = req.target.clone() else {
            return Err(NovaLspError::InvalidParams(
                "`target` must be provided for Bazel projects".to_string(),
            ));
        };

        let cache_path = workspace_root.join(".nova-cache/bazel.json");
        let runner = nova_build_bazel::DefaultCommandRunner::default();
        let mut workspace = nova_build_bazel::BazelWorkspace::new(workspace_root.clone(), runner)
            .and_then(|ws| ws.with_cache_path(cache_path))
            .map_err(|err| NovaLspError::Internal(err.to_string()))?;

        let info = workspace
            .target_compile_info(&target)
            .map_err(|err| NovaLspError::Internal(err.to_string()))?;

        let result = TargetClasspathResult {
            project_root: workspace_root.to_string_lossy().to_string(),
            target: Some(target),
            classpath: info.classpath,
            module_path: info.module_path,
            source_roots: info.source_roots,
            source: info.source,
            target_version: info.target,
        };
        serde_json::to_value(result).map_err(|err| NovaLspError::Internal(err.to_string()))
    } else {
        let config = nova_project::load_project(&requested_root)
            .map_err(|err| NovaLspError::InvalidParams(err.to_string()))?;

        let result = TargetClasspathResult {
            project_root: config.workspace_root.to_string_lossy().to_string(),
            target: req.target,
            classpath: config
                .classpath
                .iter()
                .map(|entry| entry.path.to_string_lossy().to_string())
                .collect(),
            module_path: Vec::new(),
            source_roots: config
                .source_roots
                .iter()
                .map(|root| root.path.to_string_lossy().to_string())
                .collect(),
            source: Some(config.java.source.0.to_string()),
            target_version: Some(config.java.target.0.to_string()),
        };
        serde_json::to_value(result).map_err(|err| NovaLspError::Internal(err.to_string()))
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildStatusParams {
    #[serde(alias = "root")]
    pub project_root: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildStatus {
    Idle,
    Building,
    Failed,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildStatusResult {
    pub status: BuildStatus,
}

pub fn handle_build_status(params: serde_json::Value) -> Result<serde_json::Value> {
    let _req: BuildStatusParams = serde_json::from_value(params)
        .map_err(|err| NovaLspError::InvalidParams(err.to_string()))?;

    serde_json::to_value(BuildStatusResult {
        status: BuildStatus::Idle,
    })
    .map_err(|err| NovaLspError::Internal(err.to_string()))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildDiagnosticsParams {
    #[serde(alias = "root")]
    pub project_root: String,
    #[serde(default)]
    pub target: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildDiagnosticsResult {
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub diagnostics: Vec<NovaDiagnostic>,
}

pub fn handle_build_diagnostics(params: serde_json::Value) -> Result<serde_json::Value> {
    let req: BuildDiagnosticsParams = serde_json::from_value(params)
        .map_err(|err| NovaLspError::InvalidParams(err.to_string()))?;

    let requested_root = PathBuf::from(&req.project_root);
    let requested_root = requested_root
        .canonicalize()
        .unwrap_or_else(|_| requested_root.clone());

    if nova_project::bazel_workspace_root(&requested_root).is_some() {
        // Bazel build diagnostics are expected to be sourced via `bazel build` output or BSP.
        // For now, return an empty set so clients can rely on the endpoint.
        return serde_json::to_value(BuildDiagnosticsResult {
            target: req.target,
            diagnostics: Vec::new(),
        })
        .map_err(|err| NovaLspError::Internal(err.to_string()));
    }

    // Maven/Gradle: run an incremental build and return diagnostics from the build layer.
    let params = NovaProjectParams {
        project_root: requested_root.to_string_lossy().to_string(),
        build_tool: None,
        module: None,
        project_path: None,
    };
    let manager = build_manager(&params);
    let result = run_build(&manager, &params)?;
    let resp = BuildDiagnosticsResult {
        target: req.target,
        diagnostics: result
            .diagnostics
            .into_iter()
            .map(NovaDiagnostic::from)
            .collect(),
    };
    serde_json::to_value(resp).map_err(|err| NovaLspError::Internal(err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_accepts_project_root_aliases() {
        let params: NovaProjectParams = serde_json::from_value(serde_json::json!({
            "root": "/tmp/project",
            "kind": "maven",
            "project_path": ":app",
        }))
        .unwrap();

        assert_eq!(params.project_root, "/tmp/project");
        assert_eq!(params.build_tool, Some(BuildTool::Maven));
        assert_eq!(params.project_path.as_deref(), Some(":app"));
    }
}

