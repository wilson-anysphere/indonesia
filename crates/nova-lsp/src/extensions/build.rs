use crate::{NovaLspError, Result};
use nova_build::{BuildError, BuildManager, BuildResult, Classpath};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Parameters accepted by Nova's build-related extension requests.
///
/// This is intentionally loose; clients can omit `kind` to rely on auto-detect.
#[derive(Debug, Deserialize)]
pub struct NovaProjectParams {
    pub root: String,
    #[serde(default)]
    pub kind: Option<String>,
    /// For Maven projects, a path relative to `root` identifying the module.
    #[serde(default)]
    pub module: Option<String>,
    /// For Gradle projects, a Gradle project path (e.g. `:app`).
    #[serde(default)]
    pub project_path: Option<String>,
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
        .reload_project(Path::new(&params.root))
        .map_err(map_build_error)?;
    Ok(serde_json::Value::Null)
}

fn parse_params(value: serde_json::Value) -> Result<NovaProjectParams> {
    serde_json::from_value(value).map_err(|err| NovaLspError::InvalidParams(err.to_string()))
}

fn build_manager(params: &NovaProjectParams) -> BuildManager {
    let root = PathBuf::from(&params.root);
    let cache_dir = root.join(".nova").join("build-cache");
    BuildManager::new(cache_dir)
}

fn run_build(build: &BuildManager, params: &NovaProjectParams) -> Result<BuildResult> {
    let root = PathBuf::from(&params.root);
    match detect_kind(&root, params.kind.as_deref())? {
        BuildKind::Maven => build
            .build_maven(&root, params.module.as_deref().map(Path::new))
            .map_err(map_build_error),
        BuildKind::Gradle => build
            .build_gradle(&root, params.project_path.as_deref())
            .map_err(map_build_error),
    }
}

fn run_classpath(build: &BuildManager, params: &NovaProjectParams) -> Result<Classpath> {
    let root = PathBuf::from(&params.root);
    match detect_kind(&root, params.kind.as_deref())? {
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

fn detect_kind(root: &Path, explicit: Option<&str>) -> Result<BuildKind> {
    if let Some(k) = explicit {
        return match k {
            "maven" => Ok(BuildKind::Maven),
            "gradle" => Ok(BuildKind::Gradle),
            other => Err(NovaLspError::InvalidParams(format!("unknown kind {other}"))),
        };
    }

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

fn map_build_error(err: BuildError) -> NovaLspError {
    NovaLspError::Internal(err.to_string())
}
