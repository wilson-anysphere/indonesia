use crate::{NovaLspError, Result};
use nova_apt::{AptManager, AptProgressEvent, AptRunTarget, GeneratedSourcesFreshness, ProgressReporter};
use nova_config::NovaConfig;
use nova_project::{load_project_with_options, BuildSystem, LoadOptions, SourceRootKind};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::build::NovaDiagnostic;
use super::config::load_workspace_config_with_path;

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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NovaRunAnnotationProcessingParams {
    /// Workspace root on disk.
    #[serde(alias = "root")]
    pub project_root: String,

    /// For Maven projects, a path relative to `projectRoot` identifying the module.
    #[serde(default)]
    pub module: Option<String>,

    /// For Gradle projects, a Gradle project path (e.g. `:app`).
    #[serde(default, alias = "project_path")]
    pub project_path: Option<String>,

    /// For Bazel projects, a target label (e.g. `//foo:bar`).
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub module_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub module_root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_kind: Option<String>,
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
    let mut status = apt.status().map_err(map_io_error)?;

    if let Some(module_root) = selected_module_root(apt.project(), &params) {
        status
            .modules
            .retain(|module| module.module_root == module_root);
    }

    let resp = convert_status(status);
    serde_json::to_value(resp).map_err(|err| NovaLspError::Internal(err.to_string()))
}

pub fn handle_run_annotation_processing(params: serde_json::Value) -> Result<serde_json::Value> {
    let params: NovaRunAnnotationProcessingParams = serde_json::from_value(params)
        .map_err(|err| NovaLspError::InvalidParams(err.to_string()))?;
    let root = PathBuf::from(&params.project_root);

    let build = super::build_manager_for_root(&root, Duration::from_secs(300));

    let (project, config) = load_project_with_workspace_config(&root)?;
    let apt = AptManager::new(project, config);

    let mut reporter = VecProgress::default();
    let build_result = apt
        .run_annotation_processing_for_target(&build, resolve_target(&apt, &params)?, &mut reporter)
        .map_err(map_build_error)?;

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
    let workspace_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let (config, config_path) = load_workspace_config_with_path(&workspace_root);
    let mut options = LoadOptions::default();
    options.nova_config = config.clone();
    options.nova_config_path = config_path;
    let project = load_project_with_options(&workspace_root, &options)
        .map_err(|err| NovaLspError::Internal(err.to_string()))?;
    Ok((project, config))
}

fn resolve_target(
    apt: &AptManager,
    params: &NovaRunAnnotationProcessingParams,
) -> Result<AptRunTarget> {
    let build_system = apt.project().build_system;
    let target = match build_system {
        BuildSystem::Maven => params
            .module
            .as_deref()
            .map(|m| AptRunTarget::MavenModule(PathBuf::from(m)))
            .unwrap_or(AptRunTarget::Workspace),
        BuildSystem::Gradle => params
            .project_path
            .as_deref()
            .or(params.module.as_deref())
            .map(|p| AptRunTarget::GradleProject(p.to_string()))
            .unwrap_or(AptRunTarget::Workspace),
        BuildSystem::Bazel => params
            .target
            .as_deref()
            .or(params.module.as_deref())
            .map(|t| AptRunTarget::BazelTarget(t.to_string()))
            .unwrap_or(AptRunTarget::Workspace),
        BuildSystem::Simple => AptRunTarget::Workspace,
    };
    Ok(target)
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

fn selected_module_root(
    project: &nova_project::ProjectConfig,
    params: &NovaGeneratedSourcesParams,
) -> Option<PathBuf> {
    match project.build_system {
        nova_project::BuildSystem::Maven => {
            let module = params.module.as_deref().map(str::trim)?;
            if module.is_empty() || module == "." {
                None
            } else {
                Some(project.workspace_root.join(module))
            }
        }
        nova_project::BuildSystem::Gradle => {
            let path = params.project_path.as_deref().map(str::trim)?;
            if path.is_empty() || path == ":" {
                None
            } else {
                Some(
                    project
                        .workspace_root
                        .join(gradle_project_path_to_dir(path)),
                )
            }
        }
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

impl From<AptProgressEvent> for ProgressEvent {
    fn from(event: AptProgressEvent) -> Self {
        Self {
            kind: match event.kind {
                nova_apt::AptProgressEventKind::Begin => ProgressEventKind::Begin,
                nova_apt::AptProgressEventKind::Report => ProgressEventKind::Report,
                nova_apt::AptProgressEventKind::End => ProgressEventKind::End,
            },
            message: event.message,
            module_name: event.module_name,
            module_root: event.module_root.map(|p| p.to_string_lossy().to_string()),
            source_kind: event.source_kind.map(kind_string),
        }
    }
}

#[derive(Default)]
struct VecProgress {
    events: Vec<String>,
    structured_events: Vec<ProgressEvent>,
}

impl ProgressReporter for VecProgress {
    fn event(&mut self, event: AptProgressEvent) {
        self.events.push(event.message.clone());
        self.structured_events.push(event.into());
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

    #[test]
    fn selected_module_root_normalizes_maven_root_module() {
        let project = nova_project::ProjectConfig {
            workspace_root: PathBuf::from("/workspace"),
            build_system: nova_project::BuildSystem::Maven,
            java: nova_project::JavaConfig::default(),
            modules: Vec::new(),
            jpms_modules: Vec::new(),
            source_roots: Vec::new(),
            module_path: Vec::new(),
            classpath: Vec::new(),
            output_dirs: Vec::new(),
            dependencies: Vec::new(),
        };

        let params = NovaGeneratedSourcesParams {
            project_root: "/workspace".into(),
            module: Some(".".into()),
            project_path: None,
            target: None,
        };
        assert_eq!(selected_module_root(&project, &params), None);

        let params = NovaGeneratedSourcesParams {
            project_root: "/workspace".into(),
            module: Some("module-a".into()),
            project_path: None,
            target: None,
        };
        assert_eq!(
            selected_module_root(&project, &params),
            Some(PathBuf::from("/workspace/module-a"))
        );
    }

    #[test]
    fn selected_module_root_normalizes_gradle_root_project() {
        let project = nova_project::ProjectConfig {
            workspace_root: PathBuf::from("/workspace"),
            build_system: nova_project::BuildSystem::Gradle,
            java: nova_project::JavaConfig::default(),
            modules: Vec::new(),
            jpms_modules: Vec::new(),
            source_roots: Vec::new(),
            module_path: Vec::new(),
            classpath: Vec::new(),
            output_dirs: Vec::new(),
            dependencies: Vec::new(),
        };

        let params = NovaGeneratedSourcesParams {
            project_root: "/workspace".into(),
            module: None,
            project_path: Some(":".into()),
            target: None,
        };
        assert_eq!(selected_module_root(&project, &params), None);

        let params = NovaGeneratedSourcesParams {
            project_root: "/workspace".into(),
            module: None,
            project_path: Some(":app".into()),
            target: None,
        };
        assert_eq!(
            selected_module_root(&project, &params),
            Some(PathBuf::from("/workspace/app"))
        );

        let params = NovaGeneratedSourcesParams {
            project_root: "/workspace".into(),
            module: None,
            project_path: Some(":lib:core".into()),
            target: None,
        };
        assert_eq!(
            selected_module_root(&project, &params),
            Some(PathBuf::from("/workspace/lib/core"))
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_workspace_config_instead_of_defaults() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/Hello.java"), "class Hello {}").unwrap();
        let generated = dir.path().join("target/generated-sources/annotations");
        std::fs::create_dir_all(&generated).unwrap();
        std::fs::write(
            dir.path().join("nova.toml"),
            "[generated_sources]\nenabled = false\n",
        )
        .unwrap();

        let (project, config) = load_project_with_workspace_config(dir.path()).unwrap();

        assert!(
            !config.generated_sources.enabled,
            "expected config to be loaded from nova.toml"
        );
        assert!(
            !project
                .source_roots
                .iter()
                .any(|root| root.path == generated),
            "expected generated source roots to be excluded when disabled via config"
        );
    }
}
