use crate::{NovaLspError, Result};
use nova_apt::{
    AptManager, AptProgressEvent, AptRunStatus, AptRunTarget, GeneratedSourcesFreshness,
    ProgressReporter,
};
use nova_config::NovaConfig;
use nova_project::{load_project_with_options, BuildSystem, LoadOptions, SourceRootKind};
use nova_scheduler::CancellationToken;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::build::{build_diagnostic_value, BuildStatusGuard};
use super::config::load_workspace_config_with_path;

#[derive(Debug, Clone)]
struct AptParams {
    project_root: String,
    module: Option<String>,
    project_path: Option<String>,
    target: Option<String>,
}

fn string_array_value(values: Vec<String>) -> Value {
    Value::Array(values.into_iter().map(Value::String).collect())
}

fn opt_string_value(value: Option<String>) -> Value {
    match value {
        Some(value) => Value::String(value),
        None => Value::Null,
    }
}

fn generated_sources_status_value(status: nova_apt::GeneratedSourcesStatus) -> Value {
    Value::Object({
        let mut value = serde_json::Map::new();
        value.insert("enabled".to_string(), Value::Bool(status.enabled));
        value.insert(
            "modules".to_string(),
            Value::Array(
                status
                    .modules
                    .into_iter()
                    .map(|module| {
                        Value::Object({
                            let mut module_value = serde_json::Map::new();
                            module_value.insert(
                                "moduleName".to_string(),
                                Value::String(module.module_name),
                            );
                            module_value.insert(
                                "moduleRoot".to_string(),
                                Value::String(module.module_root.to_string_lossy().to_string()),
                            );
                            module_value.insert(
                                "roots".to_string(),
                                Value::Array(
                                    module
                                        .roots
                                        .into_iter()
                                        .map(|root| {
                                            Value::Object({
                                                let mut root_value = serde_json::Map::new();
                                                root_value.insert(
                                                    "kind".to_string(),
                                                    Value::String(kind_string(root.root.kind)),
                                                );
                                                root_value.insert(
                                                    "path".to_string(),
                                                    Value::String(
                                                        root.root
                                                            .path
                                                            .to_string_lossy()
                                                            .to_string(),
                                                    ),
                                                );
                                                root_value.insert(
                                                    "freshness".to_string(),
                                                    Value::String(freshness_string(root.freshness)),
                                                );
                                                root_value
                                            })
                                        })
                                        .collect(),
                                ),
                            );
                            module_value
                        })
                    })
                    .collect(),
            ),
        );
        value
    })
}

fn progress_event_kind_string(kind: nova_apt::AptProgressEventKind) -> &'static str {
    match kind {
        nova_apt::AptProgressEventKind::Begin => "begin",
        nova_apt::AptProgressEventKind::Report => "report",
        nova_apt::AptProgressEventKind::End => "end",
    }
}

fn progress_event_value(event: AptProgressEvent) -> Value {
    Value::Object({
        let mut value = serde_json::Map::new();
        value.insert(
            "kind".to_string(),
            Value::String(progress_event_kind_string(event.kind).to_string()),
        );
        value.insert("message".to_string(), Value::String(event.message));
        if let Some(module_name) = event.module_name {
            value.insert("moduleName".to_string(), Value::String(module_name));
        }
        if let Some(module_root) = event.module_root {
            value.insert(
                "moduleRoot".to_string(),
                Value::String(module_root.to_string_lossy().to_string()),
            );
        }
        if let Some(source_kind) = event.source_kind {
            value.insert(
                "sourceKind".to_string(),
                Value::String(kind_string(source_kind)),
            );
        }
        value
    })
}

fn module_build_diagnostics_value(
    project: &nova_project::ProjectConfig,
    diagnostics: &[nova_core::BuildDiagnostic],
) -> Vec<Value> {
    use std::collections::BTreeMap;

    let mut by_module: BTreeMap<usize, Vec<Value>> = BTreeMap::new();
    for diag in diagnostics {
        let Some(module_idx) = module_index_for_file(&diag.file, &project.modules) else {
            continue;
        };
        by_module
            .entry(module_idx)
            .or_default()
            .push(build_diagnostic_value(diag.clone()));
    }

    by_module
        .into_iter()
        .map(|(idx, diags)| {
            let module = &project.modules[idx];
            Value::Object({
                let mut value = serde_json::Map::new();
                value.insert("moduleName".to_string(), Value::String(module.name.clone()));
                value.insert(
                    "moduleRoot".to_string(),
                    Value::String(module.root.to_string_lossy().to_string()),
                );
                value.insert("diagnostics".to_string(), Value::Array(diags));
                value
            })
        })
        .collect()
}

pub fn handle_generated_sources(
    params: serde_json::Value,
    cancel: CancellationToken,
) -> Result<serde_json::Value> {
    let params = parse_params(params)?;
    let root = PathBuf::from(&params.project_root);

    let build =
        super::build_manager_for_root_with_cancel(&root, Duration::from_secs(60), Some(cancel));
    let (project, config) = load_project_with_workspace_config(&root)?;
    let mut apt = AptManager::new(project, config);
    let mut status_guard = BuildStatusGuard::new(&root);
    let status_result: Result<_> = apt.status_with_build(&build).map_err(map_io_error);
    match &status_result {
        Ok(result) => {
            if let Some(err) = result.build_metadata_error.as_ref() {
                status_guard.mark_failure(Some(err.clone()));
            } else {
                status_guard.mark_success();
            }
        }
        Err(err) => status_guard.mark_failure(Some(err.to_string())),
    }
    let mut status = status_result?.status;

    if let Some(module_root) = selected_module_root(apt.project(), &params) {
        status
            .modules
            .retain(|module| module.module_root == module_root);
    }

    Ok(generated_sources_status_value(status))
}

pub fn handle_run_annotation_processing(
    params: serde_json::Value,
    cancel: CancellationToken,
) -> Result<serde_json::Value> {
    let params = parse_params(params)?;
    let root = PathBuf::from(&params.project_root);

    let build = super::build_manager_for_root_with_cancel(
        &root,
        Duration::from_secs(300),
        Some(cancel.clone()),
    );

    let (project, config) = load_project_with_workspace_config(&root)?;
    let mut apt = AptManager::new(project, config);
    let target = resolve_target(&apt, &params)?;

    let mut reporter = VecProgress::default();
    let run_result = {
        let mut status_guard = BuildStatusGuard::new(&root);
        let run_result = apt.run(&build, target, Some(cancel), &mut reporter);
        match &run_result {
            Ok(result) => {
                let has_errors = result
                    .diagnostics
                    .iter()
                    .any(|diag| matches!(diag.severity, nova_core::BuildDiagnosticSeverity::Error));
                if matches!(result.status, AptRunStatus::Failed) || has_errors {
                    let first_error = result
                        .diagnostics
                        .iter()
                        .find(|diag| {
                            matches!(diag.severity, nova_core::BuildDiagnosticSeverity::Error)
                        })
                        .map(|diag| diag.message.clone());
                    status_guard.mark_failure(
                        first_error
                            .or_else(|| result.error.clone())
                            .or_else(|| Some("annotation processing failed".to_string())),
                    );
                } else if matches!(result.status, AptRunStatus::Cancelled) {
                    status_guard.mark_failure(Some("annotation processing cancelled".to_string()));
                } else {
                    status_guard.mark_success();
                }
            }
            Err(err) => status_guard.mark_failure(Some(err.to_string())),
        }
        run_result.map_err(map_io_error)?
    };

    let module_diagnostics = module_build_diagnostics_value(apt.project(), &run_result.diagnostics);

    Ok(Value::Object({
        let mut resp = serde_json::Map::new();
        resp.insert("progress".to_string(), string_array_value(reporter.events));
        resp.insert(
            "progressEvents".to_string(),
            Value::Array(reporter.structured_events),
        );
        resp.insert(
            "diagnostics".to_string(),
            Value::Array(
                run_result
                    .diagnostics
                    .iter()
                    .cloned()
                    .map(build_diagnostic_value)
                    .collect(),
            ),
        );
        resp.insert(
            "moduleDiagnostics".to_string(),
            Value::Array(module_diagnostics),
        );
        resp.insert(
            "generatedSources".to_string(),
            generated_sources_status_value(run_result.generated_sources),
        );
        resp.insert(
            "status".to_string(),
            Value::String(status_string(run_result.status)),
        );
        resp.insert("cacheHit".to_string(), Value::Bool(run_result.cache_hit));
        resp.insert("error".to_string(), opt_string_value(run_result.error));
        resp
    }))
}

fn parse_params(value: serde_json::Value) -> Result<AptParams> {
    let obj = value
        .as_object()
        .ok_or_else(|| NovaLspError::InvalidParams("params must be an object".to_string()))?;
    let project_root = super::decode_project_root(Value::Object(obj.clone()))?;
    let module = super::get_str(obj, &["module"]).map(|s| s.to_string());
    let project_path = super::get_str(obj, &["projectPath", "project_path"]).map(|s| s.to_string());
    let target = super::get_str(obj, &["target"]).map(|s| s.to_string());
    Ok(AptParams {
        project_root,
        module,
        project_path,
        target,
    })
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

fn resolve_target(apt: &AptManager, params: &AptParams) -> Result<AptRunTarget> {
    let build_system = apt.project().build_system;
    let target = match build_system {
        BuildSystem::Maven => params
            .module
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty() && *m != ".")
            .map(|m| AptRunTarget::MavenModule(PathBuf::from(m)))
            .unwrap_or(AptRunTarget::Workspace),
        BuildSystem::Gradle => params
            .project_path
            .as_deref()
            .or(params.module.as_deref())
            .map(str::trim)
            .filter(|p| !p.is_empty() && *p != ":")
            .map(|p| {
                let path = if p.starts_with(':') {
                    p.to_string()
                } else {
                    format!(":{p}")
                };
                AptRunTarget::GradleProject(path)
            })
            .unwrap_or(AptRunTarget::Workspace),
        BuildSystem::Bazel => params
            .target
            .as_deref()
            .or(params.module.as_deref())
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(|t| AptRunTarget::BazelTarget(t.to_string()))
            .unwrap_or(AptRunTarget::Workspace),
        BuildSystem::Simple => AptRunTarget::Workspace,
    };
    Ok(target)
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

fn status_string(status: AptRunStatus) -> String {
    match status {
        AptRunStatus::UpToDate => "up_to_date".to_string(),
        AptRunStatus::Ran => "ran".to_string(),
        AptRunStatus::Cancelled => "cancelled".to_string(),
        AptRunStatus::Failed => "failed".to_string(),
    }
}

fn map_io_error(err: std::io::Error) -> NovaLspError {
    NovaLspError::Internal(err.to_string())
}

fn selected_module_root(
    project: &nova_project::ProjectConfig,
    params: &AptParams,
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
            // `module` is a legacy alias for Gradle `projectPath` (similar to how
            // `resolve_target` accepts it). Prefer the dedicated `projectPath` field but
            // fall back to `module` for backwards compatibility with older clients.
            let path = params
                .project_path
                .as_deref()
                .or(params.module.as_deref())
                .map(str::trim)?;
            super::gradle::resolve_gradle_module_root(&project.workspace_root, path)
        }
        nova_project::BuildSystem::Bazel | nova_project::BuildSystem::Simple => None,
    }
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
    structured_events: Vec<Value>,
}

impl ProgressReporter for VecProgress {
    fn event(&mut self, event: AptProgressEvent) {
        self.events.push(event.message.clone());
        self.structured_events.push(progress_event_value(event));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_accepts_project_root_aliases() {
        let params = parse_params(serde_json::Value::Object({
            let mut obj = serde_json::Map::new();
            obj.insert(
                "root".to_string(),
                serde_json::Value::String("/tmp/project".to_string()),
            );
            obj
        }))
        .unwrap();

        assert_eq!(params.project_root, "/tmp/project");
        assert!(params.module.is_none());
        assert!(params.project_path.is_none());
        assert!(params.target.is_none());
    }

    #[test]
    fn run_annotation_processing_response_includes_new_fields() {
        let value = Value::Object({
            let mut resp = serde_json::Map::new();
            resp.insert(
                "progress".to_string(),
                string_array_value(vec!["Running annotation processing".to_string()]),
            );
            resp.insert("progressEvents".to_string(), Value::Array(Vec::new()));
            resp.insert("diagnostics".to_string(), Value::Array(Vec::new()));
            resp.insert("moduleDiagnostics".to_string(), Value::Array(Vec::new()));
            resp.insert(
                "generatedSources".to_string(),
                Value::Object({
                    let mut generated_sources = serde_json::Map::new();
                    generated_sources.insert("enabled".to_string(), Value::Bool(true));
                    generated_sources.insert("modules".to_string(), Value::Array(Vec::new()));
                    generated_sources
                }),
            );
            resp.insert(
                "status".to_string(),
                Value::String("up_to_date".to_string()),
            );
            resp.insert("cacheHit".to_string(), Value::Bool(false));
            resp.insert("error".to_string(), Value::Null);
            resp
        });
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
            jpms_workspace: None,
            source_roots: Vec::new(),
            module_path: Vec::new(),
            classpath: Vec::new(),
            output_dirs: Vec::new(),
            dependencies: Vec::new(),
            workspace_model: None,
        };

        let params = AptParams {
            project_root: "/workspace".into(),
            module: Some(".".into()),
            project_path: None,
            target: None,
        };
        assert_eq!(selected_module_root(&project, &params), None);

        let params = AptParams {
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
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("settings.gradle"),
            "include ':app', ':lib:core'\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("app")).unwrap();
        std::fs::create_dir_all(dir.path().join("lib/core")).unwrap();

        let (project, _config) = load_project_with_workspace_config(dir.path()).unwrap();

        let params = AptParams {
            project_root: dir.path().to_string_lossy().to_string(),
            module: None,
            project_path: Some(":".into()),
            target: None,
        };
        assert_eq!(selected_module_root(&project, &params), None);

        let params = AptParams {
            project_root: dir.path().to_string_lossy().to_string(),
            module: None,
            project_path: Some(":app".into()),
            target: None,
        };
        assert_eq!(
            selected_module_root(&project, &params),
            Some(dir.path().join("app").canonicalize().unwrap())
        );

        let params = AptParams {
            project_root: dir.path().to_string_lossy().to_string(),
            module: None,
            project_path: Some(":lib:core".into()),
            target: None,
        };
        assert_eq!(
            selected_module_root(&project, &params),
            Some(dir.path().join("lib/core").canonicalize().unwrap())
        );
    }

    #[test]
    fn selected_module_root_resolves_gradle_project_dir_override() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("settings.gradle"),
            "include ':app'\nproject(':app').projectDir = file('modules/application')\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("modules/application")).unwrap();

        let (project, _config) = load_project_with_workspace_config(dir.path()).unwrap();

        let params = AptParams {
            project_root: dir.path().to_string_lossy().to_string(),
            module: None,
            project_path: Some(":app".into()),
            target: None,
        };

        assert_eq!(
            selected_module_root(&project, &params),
            Some(
                dir.path()
                    .join("modules/application")
                    .canonicalize()
                    .unwrap()
            )
        );
    }

    #[test]
    fn selected_module_root_resolves_gradle_include_flat_outside_workspace_root() {
        let dir = tempfile::tempdir().unwrap();
        let workspace_root = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace_root).unwrap();
        std::fs::write(
            workspace_root.join("settings.gradle"),
            "includeFlat 'app'\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("app")).unwrap();

        let (project, _config) = load_project_with_workspace_config(&workspace_root).unwrap();

        let params = AptParams {
            project_root: workspace_root.to_string_lossy().to_string(),
            module: None,
            project_path: Some(":app".into()),
            target: None,
        };

        assert_eq!(
            selected_module_root(&project, &params),
            Some(dir.path().join("app").canonicalize().unwrap())
        );
    }

    #[test]
    fn selected_module_root_accepts_module_alias_for_gradle_project_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("settings.gradle"),
            "include ':app'\nproject(':app').projectDir = file('modules/application')\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("modules/application")).unwrap();

        let (project, _config) = load_project_with_workspace_config(dir.path()).unwrap();

        let params = AptParams {
            project_root: dir.path().to_string_lossy().to_string(),
            // Legacy clients may send `module` instead of `projectPath` for Gradle.
            module: Some(":app".into()),
            project_path: None,
            target: None,
        };

        assert_eq!(
            selected_module_root(&project, &params),
            Some(
                dir.path()
                    .join("modules/application")
                    .canonicalize()
                    .unwrap()
            )
        );
    }

    #[test]
    fn resolve_target_normalizes_maven_root_module() {
        let project = nova_project::ProjectConfig {
            workspace_root: PathBuf::from("/workspace"),
            build_system: nova_project::BuildSystem::Maven,
            java: nova_project::JavaConfig::default(),
            modules: Vec::new(),
            jpms_modules: Vec::new(),
            jpms_workspace: None,
            source_roots: Vec::new(),
            module_path: Vec::new(),
            classpath: Vec::new(),
            output_dirs: Vec::new(),
            dependencies: Vec::new(),
            workspace_model: None,
        };

        let apt = AptManager::new(project, NovaConfig::default());
        let params = AptParams {
            project_root: "/workspace".into(),
            module: Some(".".into()),
            project_path: None,
            target: None,
        };

        assert_eq!(
            resolve_target(&apt, &params).unwrap(),
            AptRunTarget::Workspace
        );
    }

    #[test]
    fn resolve_target_normalizes_gradle_root_project() {
        let project = nova_project::ProjectConfig {
            workspace_root: PathBuf::from("/workspace"),
            build_system: nova_project::BuildSystem::Gradle,
            java: nova_project::JavaConfig::default(),
            modules: Vec::new(),
            jpms_modules: Vec::new(),
            jpms_workspace: None,
            source_roots: Vec::new(),
            module_path: Vec::new(),
            classpath: Vec::new(),
            output_dirs: Vec::new(),
            dependencies: Vec::new(),
            workspace_model: None,
        };

        let apt = AptManager::new(project, NovaConfig::default());
        let params = AptParams {
            project_root: "/workspace".into(),
            module: None,
            project_path: Some(":".into()),
            target: None,
        };

        assert_eq!(
            resolve_target(&apt, &params).unwrap(),
            AptRunTarget::Workspace
        );
    }

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
