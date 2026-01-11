use crate::{NovaLspError, Result};
use nova_apt::{AptManager, GeneratedSourcesFreshness, ProgressReporter};
use nova_build::BuildManager;
use nova_config::NovaConfig;
use nova_project::{load_project_with_options, LoadOptions, SourceRootKind};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use super::build::NovaDiagnostic;
use super::config::load_workspace_config;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NovaGeneratedSourcesParams {
    /// Workspace root on disk.
    ///
    /// Clients should prefer `projectRoot` (camelCase). `root` is accepted for
    /// backwards compatibility with early experiments.
    #[serde(alias = "root")]
    pub project_root: String,

    /// For Maven projects, a path relative to `projectRoot` identifying the module.
    #[serde(default)]
    pub module: Option<String>,
    /// For Gradle projects, a Gradle project path (e.g. `:app`).
    #[serde(default, alias = "project_path")]
    pub project_path: Option<String>,
    /// For Bazel projects, a Bazel target label (e.g. `//app:lib`).
    #[serde(default)]
    pub target: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GeneratedSourceRootInfo {
    pub kind: String,
    pub path: String,
    pub freshness: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModuleGeneratedSourcesInfo {
    pub module_name: String,
    pub module_root: String,
    pub roots: Vec<GeneratedSourceRootInfo>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GeneratedSourcesResponse {
    pub enabled: bool,
    pub modules: Vec<ModuleGeneratedSourcesInfo>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunAnnotationProcessingResponse {
    pub progress: Vec<String>,
    #[serde(default)]
    pub progress_events: Vec<ProgressEvent>,
    pub diagnostics: Vec<NovaDiagnostic>,
    #[serde(default)]
    pub module_diagnostics: Vec<ModuleBuildDiagnostics>,
    pub generated_sources: GeneratedSourcesResponse,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressEvent {
    pub kind: ProgressEventKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressEventKind {
    Begin,
    Report,
    End,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModuleBuildDiagnostics {
    pub module_name: String,
    pub module_root: String,
    pub diagnostics: Vec<NovaDiagnostic>,
}

pub fn handle_generated_sources(params: serde_json::Value) -> Result<serde_json::Value> {
    let params = parse_params(params)?;
    let root = PathBuf::from(&params.project_root);

    let (project, config) = load_project_with_workspace_config(&root)?;
    let apt = AptManager::new(project, config);
    let status = apt.status().map_err(map_io_error)?;

    let mut resp = convert_status(status);
    if let Some(module_root) = selected_module_root(&apt, &params) {
        let module_root = module_root.to_string_lossy().to_string();
        resp.modules
            .retain(|module| module.module_root == module_root);
    }

    serde_json::to_value(resp).map_err(|err| NovaLspError::Internal(err.to_string()))
}

pub fn handle_run_annotation_processing(params: serde_json::Value) -> Result<serde_json::Value> {
    let params = parse_params(params)?;
    let root = PathBuf::from(&params.project_root);

    let cache_dir = root.join(".nova").join("build-cache");
    let build = BuildManager::new(cache_dir);

    let (project, config) = load_project_with_workspace_config(&root)?;
    let apt = AptManager::new(project, config);

    let mut reporter = VecProgress::default();
    reporter.begin("Running annotation processing");
    reporter.report("Invoking build tool");

    let build_result = match apt.project().build_system {
        nova_project::BuildSystem::Maven => build
            .build_maven(
                &apt.project().workspace_root,
                params.module.as_deref().map(Path::new),
            )
            .map_err(map_build_error)?,
        nova_project::BuildSystem::Gradle => build
            .build_gradle(
                &apt.project().workspace_root,
                params.project_path.as_deref(),
            )
            .map_err(map_build_error)?,
        nova_project::BuildSystem::Bazel | nova_project::BuildSystem::Simple => {
            nova_build::BuildResult {
                diagnostics: Vec::new(),
            }
        }
    };

    reporter.report("Build finished");
    reporter.end();

    // Reload project + generated roots after the build.
    let (project, config) = load_project_with_workspace_config(&root)?;
    let apt = AptManager::new(project, config);
    let status = apt.status().map_err(map_io_error)?;

    let module_diagnostics = group_diagnostics_by_module(&build_result.diagnostics, apt.project());

    let resp = RunAnnotationProcessingResponse {
        progress: reporter.events,
        progress_events: reporter.structured_events,
        diagnostics: build_result
            .diagnostics
            .into_iter()
            .map(NovaDiagnostic::from)
            .collect(),
        module_diagnostics,
        generated_sources: convert_status(status),
    };

    serde_json::to_value(resp).map_err(|err| NovaLspError::Internal(err.to_string()))
}

fn parse_params(value: serde_json::Value) -> Result<NovaGeneratedSourcesParams> {
    serde_json::from_value(value).map_err(|err| NovaLspError::InvalidParams(err.to_string()))
}

fn load_project_with_workspace_config(
    root: &Path,
) -> Result<(nova_project::ProjectConfig, NovaConfig)> {
    let config = load_workspace_config(root);
    let mut options = LoadOptions::default();
    options.nova_config = config.clone();
    let project = load_project_with_options(root, &options)
        .map_err(|err| NovaLspError::Internal(err.to_string()))?;
    Ok((project, config))
}

fn convert_status(status: nova_apt::GeneratedSourcesStatus) -> GeneratedSourcesResponse {
    GeneratedSourcesResponse {
        enabled: status.enabled,
        modules: status
            .modules
            .into_iter()
            .map(|module| ModuleGeneratedSourcesInfo {
                module_name: module.module_name,
                module_root: module.module_root.to_string_lossy().to_string(),
                roots: module
                    .roots
                    .into_iter()
                    .map(|root| GeneratedSourceRootInfo {
                        kind: kind_string(root.root.kind),
                        path: root.root.path.to_string_lossy().to_string(),
                        freshness: freshness_string(root.freshness),
                    })
                    .collect(),
            })
            .collect(),
    }
}

fn kind_string(kind: SourceRootKind) -> String {
    match kind {
        SourceRootKind::Main => "main".to_string(),
        SourceRootKind::Test => "test".to_string(),
    }
}

fn freshness_string(freshness: GeneratedSourcesFreshness) -> String {
    match freshness {
        GeneratedSourcesFreshness::Missing => "missing".to_string(),
        GeneratedSourcesFreshness::Stale => "stale".to_string(),
        GeneratedSourcesFreshness::Fresh => "fresh".to_string(),
    }
}

fn map_build_error(err: nova_build::BuildError) -> NovaLspError {
    NovaLspError::Internal(err.to_string())
}

fn map_io_error(err: std::io::Error) -> NovaLspError {
    NovaLspError::Internal(err.to_string())
}

fn selected_module_root(apt: &AptManager, params: &NovaGeneratedSourcesParams) -> Option<PathBuf> {
    match apt.project().build_system {
        nova_project::BuildSystem::Maven => params.module.as_deref().and_then(|module| {
            let module = module.trim();
            if module.is_empty() || module == "." {
                Some(apt.project().workspace_root.clone())
            } else {
                Some(apt.project().workspace_root.join(module))
            }
        }),
        nova_project::BuildSystem::Gradle => params.project_path.as_deref().map(|path| {
            apt.project()
                .workspace_root
                .join(gradle_project_path_to_dir(path))
        }),
        nova_project::BuildSystem::Bazel | nova_project::BuildSystem::Simple => None,
    }
}

fn gradle_project_path_to_dir(project_path: &str) -> PathBuf {
    let trimmed = project_path.trim_matches(':');
    let mut rel = PathBuf::new();
    for part in trimmed.split(':').filter(|p| !p.is_empty()) {
        rel.push(part);
    }
    rel
}

fn group_diagnostics_by_module(
    diagnostics: &[nova_core::Diagnostic],
    project: &nova_project::ProjectConfig,
) -> Vec<ModuleBuildDiagnostics> {
    use std::collections::BTreeMap;

    let mut by_module: BTreeMap<usize, Vec<NovaDiagnostic>> = BTreeMap::new();

    for diag in diagnostics {
        let Some(module_idx) = module_index_for_file(&diag.file, &project.modules) else {
            continue;
        };
        by_module
            .entry(module_idx)
            .or_default()
            .push(NovaDiagnostic::from(diag.clone()));
    }

    by_module
        .into_iter()
        .map(|(idx, diags)| {
            let module = &project.modules[idx];
            ModuleBuildDiagnostics {
                module_name: module.name.clone(),
                module_root: module.root.to_string_lossy().to_string(),
                diagnostics: diags,
            }
        })
        .collect()
}

fn module_index_for_file(file: &Path, modules: &[nova_project::Module]) -> Option<usize> {
    modules
        .iter()
        .enumerate()
        .filter(|(_, module)| file.starts_with(&module.root))
        .max_by_key(|(_, module)| module.root.components().count())
        .map(|(idx, _)| idx)
}

#[derive(Default)]
struct VecProgress {
    events: Vec<String>,
    structured_events: Vec<ProgressEvent>,
}

impl ProgressReporter for VecProgress {
    fn begin(&mut self, title: &str) {
        self.events.push(title.to_string());
        self.structured_events.push(ProgressEvent {
            kind: ProgressEventKind::Begin,
            message: title.to_string(),
        });
    }

    fn report(&mut self, message: &str) {
        self.events.push(message.to_string());
        self.structured_events.push(ProgressEvent {
            kind: ProgressEventKind::Report,
            message: message.to_string(),
        });
    }

    fn end(&mut self) {
        self.events.push("done".to_string());
        self.structured_events.push(ProgressEvent {
            kind: ProgressEventKind::End,
            message: "done".to_string(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_accepts_project_root_aliases() {
        let params: NovaGeneratedSourcesParams = serde_json::from_value(serde_json::json!({
            "root": "/tmp/project",
        }))
        .unwrap();

        assert_eq!(params.project_root, "/tmp/project");
        assert!(params.module.is_none());
        assert!(params.project_path.is_none());
        assert!(params.target.is_none());
    }

    #[test]
    fn run_annotation_processing_response_includes_new_fields() {
        let resp = RunAnnotationProcessingResponse {
            progress: vec!["Running annotation processing".to_string()],
            progress_events: Vec::new(),
            diagnostics: Vec::new(),
            module_diagnostics: Vec::new(),
            generated_sources: GeneratedSourcesResponse {
                enabled: true,
                modules: Vec::new(),
            },
        };

        let value = serde_json::to_value(resp).unwrap();
        assert!(value.get("progress").is_some());
        assert!(value.get("progressEvents").is_some());
        assert!(value.get("moduleDiagnostics").is_some());
    }
}
