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
    pub diagnostics: Vec<NovaDiagnostic>,
    pub generated_sources: GeneratedSourcesResponse,
}

pub fn handle_generated_sources(params: serde_json::Value) -> Result<serde_json::Value> {
    let params = parse_params(params)?;
    let root = PathBuf::from(&params.project_root);

    let (project, config) = load_project_with_workspace_config(&root)?;
    let apt = AptManager::new(project, config);
    let status = apt.status().map_err(map_io_error)?;

    serde_json::to_value(convert_status(status))
        .map_err(|err| NovaLspError::Internal(err.to_string()))
}

pub fn handle_run_annotation_processing(params: serde_json::Value) -> Result<serde_json::Value> {
    let params = parse_params(params)?;
    let root = PathBuf::from(&params.project_root);

    let cache_dir = root.join(".nova").join("build-cache");
    let build = BuildManager::new(cache_dir);

    let (project, config) = load_project_with_workspace_config(&root)?;
    let apt = AptManager::new(project, config);

    let mut reporter = VecProgress::default();
    let build_result = apt
        .run_annotation_processing(&build, &mut reporter)
        .map_err(map_build_error)?;

    // Reload project + generated roots after the build.
    let (project, config) = load_project_with_workspace_config(&root)?;
    let apt = AptManager::new(project, config);
    let status = apt.status().map_err(map_io_error)?;

    let resp = RunAnnotationProcessingResponse {
        progress: reporter.events,
        diagnostics: build_result
            .diagnostics
            .into_iter()
            .map(NovaDiagnostic::from)
            .collect(),
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

#[derive(Default)]
struct VecProgress {
    events: Vec<String>,
}

impl ProgressReporter for VecProgress {
    fn begin(&mut self, title: &str) {
        self.events.push(title.to_string());
    }

    fn report(&mut self, message: &str) {
        self.events.push(message.to_string());
    }

    fn end(&mut self) {
        self.events.push("done".to_string());
    }
}
