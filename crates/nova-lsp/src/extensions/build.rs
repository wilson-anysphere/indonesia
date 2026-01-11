use crate::{NovaLspError, Result};
use nova_build::{BuildError, BuildManager, BuildResult, Classpath, JavaCompileConfig};
use nova_cache::{CacheConfig, CacheDir};
use nova_project::{load_project_with_options, LoadOptions};
use serde::{Deserialize, Serialize};
use std::{
    borrow::Cow,
    env,
    path::{Path, PathBuf},
    time::Duration,
};

use super::config::load_workspace_config;

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
#[serde(rename_all = "camelCase")]
pub struct NovaClasspathResponse {
    pub classpath: Vec<String>,
    #[serde(default)]
    pub module_path: Vec<String>,
    #[serde(default)]
    pub source_roots: Vec<String>,
    #[serde(default)]
    pub generated_source_roots: Vec<String>,
    pub language_level: LanguageLevel,
    pub output_dirs: OutputDirs,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LanguageLevel {
    pub major: u16,
    #[serde(default)]
    pub preview: bool,
}

impl Default for LanguageLevel {
    fn default() -> Self {
        Self {
            // Nova's default language level elsewhere is Java 17.
            major: 17,
            preview: false,
        }
    }
}

#[derive(Debug, Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OutputDirs {
    #[serde(default)]
    pub main: Vec<String>,
    #[serde(default)]
    pub test: Vec<String>,
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
    let project_root = PathBuf::from(&params.project_root);
    let manager = super::build_manager_for_root(&project_root, Duration::from_secs(120));
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
    let project_root = PathBuf::from(&params.project_root);
    let manager = super::build_manager_for_root(&project_root, Duration::from_secs(60));
    let metadata = load_build_metadata(&params);

    let kind = detect_kind(&project_root, params.build_tool)?;
    let compile_config = match kind {
        BuildKind::Maven => manager.java_compile_config_maven(
            &project_root,
            normalize_maven_module_relative(params.module.as_deref()),
        ),
        BuildKind::Gradle => {
            let project_path = normalize_gradle_project_path(params.project_path.as_deref());
            manager.java_compile_config_gradle(&project_root, project_path.as_deref())
        }
    };

    let (classpath, module_path, source_roots, language_level, output_dirs) = match compile_config {
        Ok(cfg) => {
            let classpath = paths_to_strings(cfg.compile_classpath.iter());
            let module_path = if cfg.module_path.is_empty() {
                metadata.module_path.clone()
            } else {
                paths_to_strings(cfg.module_path.iter())
            };
            let source_roots = {
                let mut seen = std::collections::HashSet::new();
                let mut roots = Vec::new();
                for root in cfg
                    .main_source_roots
                    .iter()
                    .chain(cfg.test_source_roots.iter())
                {
                    let s = root.to_string_lossy().to_string();
                    if seen.insert(s.clone()) {
                        roots.push(s);
                    }
                }
                if roots.is_empty() {
                    metadata.source_roots.clone()
                } else {
                    roots
                }
            };
            let language_level =
                language_level_from_java_compile_config(&cfg).unwrap_or(metadata.language_level);
            let output_dirs = output_dirs_from_java_compile_config(&cfg)
                .filter(|dirs| !(dirs.main.is_empty() && dirs.test.is_empty()))
                .unwrap_or_else(|| metadata.output_dirs.clone());
            (
                classpath,
                module_path,
                source_roots,
                language_level,
                output_dirs,
            )
        }
        Err(_) => {
            // If the richer compile-config extraction fails, fall back to the legacy
            // classpath computation so existing clients keep working.
            let cp = run_classpath(&manager, &params)?;
            let classpath = paths_to_strings(cp.entries.iter());
            (
                classpath,
                metadata.module_path.clone(),
                metadata.source_roots.clone(),
                metadata.language_level,
                metadata.output_dirs.clone(),
            )
        }
    };

    let resp = NovaClasspathResponse {
        classpath,
        module_path,
        source_roots,
        generated_source_roots: metadata.generated_source_roots,
        language_level,
        output_dirs,
    };
    serde_json::to_value(resp).map_err(|err| NovaLspError::Internal(err.to_string()))
}

pub fn handle_reload_project(params: serde_json::Value) -> Result<serde_json::Value> {
    let params = parse_params(params)?;
    let project_root = PathBuf::from(&params.project_root);
    let manager = super::build_manager_for_root(&project_root, Duration::from_secs(60));
    manager
        .reload_project(&project_root)
        .map_err(map_build_error)?;
    Ok(serde_json::Value::Null)
}

fn parse_params(value: serde_json::Value) -> Result<NovaProjectParams> {
    serde_json::from_value(value).map_err(|err| NovaLspError::InvalidParams(err.to_string()))
}

fn run_build(build: &BuildManager, params: &NovaProjectParams) -> Result<BuildResult> {
    let root = PathBuf::from(&params.project_root);
    match detect_kind(&root, params.build_tool)? {
        BuildKind::Maven => build
            .build_maven(
                &root,
                normalize_maven_module_relative(params.module.as_deref()),
            )
            .map_err(map_build_error),
        BuildKind::Gradle => build
            .build_gradle(
                &root,
                normalize_gradle_project_path(params.project_path.as_deref()).as_deref(),
            )
            .map_err(map_build_error),
    }
}

fn run_classpath(build: &BuildManager, params: &NovaProjectParams) -> Result<Classpath> {
    let root = PathBuf::from(&params.project_root);
    match detect_kind(&root, params.build_tool)? {
        BuildKind::Maven => build
            .classpath_maven(
                &root,
                normalize_maven_module_relative(params.module.as_deref()),
            )
            .map_err(map_build_error),
        BuildKind::Gradle => build
            .classpath_gradle(
                &root,
                normalize_gradle_project_path(params.project_path.as_deref()).as_deref(),
            )
            .map_err(map_build_error),
    }
}

fn normalize_maven_module_relative(module: Option<&str>) -> Option<&Path> {
    let module = module.map(str::trim)?;
    if module.is_empty() || module == "." {
        None
    } else {
        Some(Path::new(module))
    }
}

enum BuildKind {
    Maven,
    Gradle,
}

fn normalize_gradle_project_path(project_path: Option<&str>) -> Option<Cow<'_, str>> {
    let project_path = project_path.map(str::trim)?;
    if project_path.is_empty() || project_path == ":" {
        return None;
    }
    if project_path.starts_with(':') {
        Some(Cow::Borrowed(project_path))
    } else {
        Some(Cow::Owned(format!(":{project_path}")))
    }
}

fn paths_to_strings<'a>(paths: impl IntoIterator<Item = &'a PathBuf>) -> Vec<String> {
    paths
        .into_iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect()
}

fn parse_java_major(text: &str) -> Option<u16> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let trimmed = trimmed.strip_prefix("1.").unwrap_or(trimmed);
    let digits: String = trimmed.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

fn language_level_from_java_compile_config(cfg: &JavaCompileConfig) -> Option<LanguageLevel> {
    let major = cfg
        .release
        .as_deref()
        .and_then(parse_java_major)
        .or_else(|| cfg.source.as_deref().and_then(parse_java_major))
        .or_else(|| cfg.target.as_deref().and_then(parse_java_major))?;
    Some(LanguageLevel {
        major,
        preview: cfg.enable_preview,
    })
}

fn output_dirs_from_java_compile_config(cfg: &JavaCompileConfig) -> Option<OutputDirs> {
    let mut out = OutputDirs::default();
    if let Some(dir) = &cfg.main_output_dir {
        out.main.push(dir.to_string_lossy().to_string());
    }
    if let Some(dir) = &cfg.test_output_dir {
        out.test.push(dir.to_string_lossy().to_string());
    }
    Some(out)
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

#[derive(Debug, Default)]
struct BuildMetadata {
    module_path: Vec<String>,
    source_roots: Vec<String>,
    generated_source_roots: Vec<String>,
    language_level: LanguageLevel,
    output_dirs: OutputDirs,
}

fn load_build_metadata(params: &NovaProjectParams) -> BuildMetadata {
    let root = PathBuf::from(&params.project_root);
    let kind = match detect_kind(&root, params.build_tool) {
        Ok(kind) => kind,
        Err(_) => return BuildMetadata::default(),
    };

    let nova_config = load_workspace_config(&root);
    let mut options = LoadOptions::default();
    options.nova_config = nova_config;
    let Ok(project) = load_project_with_options(&root, &options) else {
        return BuildMetadata::default();
    };

    let module_roots = match kind {
        BuildKind::Maven => {
            if let Some(module) = params.module.as_deref().filter(|m| !m.trim().is_empty()) {
                let module = module.trim();
                if module == "." {
                    vec![project.workspace_root.clone()]
                } else {
                    vec![project.workspace_root.join(module)]
                }
            } else {
                project.modules.iter().map(|m| m.root.clone()).collect()
            }
        }
        BuildKind::Gradle => {
            if let Some(project_path) = params
                .project_path
                .as_deref()
                .filter(|p| !p.trim().is_empty())
            {
                let rel = gradle_project_path_to_dir(project_path);
                vec![project.workspace_root.join(rel)]
            } else {
                project.modules.iter().map(|m| m.root.clone()).collect()
            }
        }
    };

    let source_roots = project
        .source_roots
        .iter()
        .filter(|root| root.origin == nova_project::SourceRootOrigin::Source)
        .filter(|root| {
            module_roots
                .iter()
                .any(|module_root| root.path.starts_with(module_root))
        })
        .map(|root| root.path.to_string_lossy().to_string())
        .collect();

    let generated_source_roots = project
        .source_roots
        .iter()
        .filter(|root| root.origin == nova_project::SourceRootOrigin::Generated)
        .filter(|root| {
            module_roots
                .iter()
                .any(|module_root| root.path.starts_with(module_root))
        })
        .map(|root| root.path.to_string_lossy().to_string())
        .collect();

    let mut output_dirs = OutputDirs::default();
    for dir in &project.output_dirs {
        if !module_roots
            .iter()
            .any(|module_root| dir.path.starts_with(module_root))
        {
            continue;
        }
        match dir.kind {
            nova_project::OutputDirKind::Main => {
                output_dirs
                    .main
                    .push(dir.path.to_string_lossy().to_string());
            }
            nova_project::OutputDirKind::Test => {
                output_dirs
                    .test
                    .push(dir.path.to_string_lossy().to_string());
            }
        }
    }

    BuildMetadata {
        module_path: project
            .module_path
            .iter()
            .map(|entry| entry.path.to_string_lossy().to_string())
            .collect(),
        source_roots,
        generated_source_roots,
        language_level: LanguageLevel {
            major: project.java.source.0,
            preview: false,
        },
        output_dirs,
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

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
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
    #[serde(default)]
    pub release: Option<String>,
    #[serde(default)]
    pub output_dir: Option<String>,
    #[serde(default)]
    pub enable_preview: bool,
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

        let cache_path = CacheDir::new(&workspace_root, CacheConfig::from_env())
            .map(|dir| dir.queries_dir().join("bazel.json"))
            .map_err(|err| NovaLspError::Internal(err.to_string()))?;
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
            release: info.release,
            output_dir: info.output_dir,
            enable_preview: info.preview,
        };
        serde_json::to_value(result).map_err(|err| NovaLspError::Internal(err.to_string()))
    } else {
        let nova_config = load_workspace_config(&requested_root);
        let mut options = LoadOptions::default();
        options.nova_config = nova_config;
        let config = load_project_with_options(&requested_root, &options)
            .map_err(|err| NovaLspError::InvalidParams(err.to_string()))?;

        let project_root = config.workspace_root.clone();
        let manager = super::build_manager_for_root(&project_root, Duration::from_secs(60));

        let normalized_target = req
            .target
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string);

        let cp = match config.build_system {
            nova_project::BuildSystem::Maven => {
                let module_relative = normalized_target
                    .as_deref()
                    .filter(|t| *t != ".")
                    .map(Path::new);
                manager
                    .classpath_maven(&project_root, module_relative)
                    .map_err(map_build_error)?
            }
            nova_project::BuildSystem::Gradle => {
                let project_path = normalized_target.as_deref().map(|t| {
                    if t.starts_with(':') {
                        t.to_string()
                    } else {
                        format!(":{t}")
                    }
                });
                let project_path = project_path.as_deref().filter(|t| *t != ":");

                manager
                    .classpath_gradle(&project_root, project_path)
                    .map_err(map_build_error)?
            }
            // For simple projects, `nova-project` is already the source of truth.
            nova_project::BuildSystem::Simple => Classpath::new(
                config
                    .classpath
                    .iter()
                    .map(|entry| entry.path.clone())
                    .collect(),
            ),
            // Bazel workspaces are handled above via `bazel_workspace_root`.
            nova_project::BuildSystem::Bazel => {
                return Err(NovaLspError::InvalidParams(
                    "Bazel workspace was not detected at the requested root".to_string(),
                ));
            }
        };

        let result = TargetClasspathResult {
            project_root: project_root.to_string_lossy().to_string(),
            target: normalized_target,
            classpath: cp
                .entries
                .into_iter()
                .map(|entry| entry.to_string_lossy().to_string())
                .collect(),
            module_path: config
                .module_path
                .iter()
                .map(|entry| entry.path.to_string_lossy().to_string())
                .collect(),
            source_roots: config
                .source_roots
                .iter()
                .map(|root| root.path.to_string_lossy().to_string())
                .collect(),
            source: Some(config.java.source.0.to_string()),
            target_version: Some(config.java.target.0.to_string()),
            release: None,
            output_dir: None,
            enable_preview: false,
        };
        serde_json::to_value(result).map_err(|err| NovaLspError::Internal(err.to_string()))
    }
}

// -----------------------------------------------------------------------------
// Unified project model (Maven/Gradle/Bazel)
// -----------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectModelParams {
    #[serde(alias = "root")]
    pub project_root: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct JavaLanguageLevel {
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub release: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectModelResult {
    pub project_root: String,
    pub units: Vec<ProjectModelUnit>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum ProjectModelUnit {
    Maven {
        /// Maven module directory relative to the workspace root (e.g. `.`, `module-a`).
        module: String,
        compile_classpath: Vec<String>,
        #[serde(default)]
        module_path: Vec<String>,
        #[serde(default)]
        source_roots: Vec<String>,
        #[serde(default)]
        language_level: Option<JavaLanguageLevel>,
    },
    Gradle {
        /// Gradle project path (e.g. `:`, `:app`, `:lib:core`).
        project_path: String,
        compile_classpath: Vec<String>,
        #[serde(default)]
        module_path: Vec<String>,
        #[serde(default)]
        source_roots: Vec<String>,
        #[serde(default)]
        language_level: Option<JavaLanguageLevel>,
    },
    Bazel {
        /// Bazel label (e.g. `//java/com/example:lib`).
        target: String,
        compile_classpath: Vec<String>,
        #[serde(default)]
        module_path: Vec<String>,
        #[serde(default)]
        source_roots: Vec<String>,
        #[serde(default)]
        language_level: Option<JavaLanguageLevel>,
    },
    Simple {
        module: String,
        compile_classpath: Vec<String>,
        #[serde(default)]
        module_path: Vec<String>,
        #[serde(default)]
        source_roots: Vec<String>,
        #[serde(default)]
        language_level: Option<JavaLanguageLevel>,
    },
}

pub fn handle_project_model(params: serde_json::Value) -> Result<serde_json::Value> {
    let req: ProjectModelParams = serde_json::from_value(params)
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
        let cache_path = CacheDir::new(&workspace_root, CacheConfig::from_env())
            .map(|dir| dir.queries_dir().join("bazel.json"))
            .map_err(|err| NovaLspError::Internal(err.to_string()))?;
        let runner = nova_build_bazel::DefaultCommandRunner::default();
        let mut workspace = nova_build_bazel::BazelWorkspace::new(workspace_root.clone(), runner)
            .and_then(|ws| ws.with_cache_path(cache_path))
            .map_err(|err| NovaLspError::Internal(err.to_string()))?;

        let targets = workspace
            .java_targets()
            .map_err(|err| NovaLspError::Internal(err.to_string()))?;

        let mut units = Vec::with_capacity(targets.len());
        for target in targets {
            let info = workspace
                .target_compile_info(&target)
                .map_err(|err| NovaLspError::Internal(err.to_string()))?;
            units.push(ProjectModelUnit::Bazel {
                target,
                compile_classpath: info.classpath,
                module_path: info.module_path,
                source_roots: info.source_roots,
                language_level: Some(JavaLanguageLevel {
                    source: info.source,
                    target: info.target,
                    release: None,
                }),
            });
        }

        let result = ProjectModelResult {
            project_root: workspace_root.to_string_lossy().to_string(),
            units,
        };
        return serde_json::to_value(result).map_err(|err| NovaLspError::Internal(err.to_string()));
    }

    let config = nova_project::load_project(&requested_root)
        .map_err(|err| NovaLspError::InvalidParams(err.to_string()))?;
    let project_root = config.workspace_root.clone();

    let manager = super::build_manager_for_root(&project_root, Duration::from_secs(120));

    let language_level = Some(JavaLanguageLevel {
        source: Some(config.java.source.0.to_string()),
        target: Some(config.java.target.0.to_string()),
        release: None,
    });

    let units = match config.build_system {
        nova_project::BuildSystem::Maven => config
            .modules
            .iter()
            .map(|module| {
                let rel = module
                    .root
                    .strip_prefix(&project_root)
                    .unwrap_or(module.root.as_path());
                let rel = if rel.as_os_str().is_empty() {
                    ".".to_string()
                } else {
                    rel.to_string_lossy().to_string()
                };

                let module_relative = if rel == "." {
                    None
                } else {
                    Some(Path::new(&rel))
                };
                let cp = manager
                    .classpath_maven(&project_root, module_relative)
                    .map_err(map_build_error)?;

                let source_roots = config
                    .source_roots
                    .iter()
                    .filter(|root| root.path.starts_with(&module.root))
                    .map(|root| root.path.to_string_lossy().to_string())
                    .collect();

                Ok(ProjectModelUnit::Maven {
                    module: rel,
                    compile_classpath: cp
                        .entries
                        .into_iter()
                        .map(|p| p.to_string_lossy().to_string())
                        .collect(),
                    module_path: Vec::new(),
                    source_roots,
                    language_level: language_level.clone(),
                })
            })
            .collect::<Result<Vec<_>>>()?,
        nova_project::BuildSystem::Gradle => config
            .modules
            .iter()
            .map(|module| {
                let rel = module
                    .root
                    .strip_prefix(&project_root)
                    .unwrap_or(module.root.as_path());
                let project_path = if rel.as_os_str().is_empty() {
                    ":".to_string()
                } else {
                    let mut out = String::from(":");
                    let mut first = true;
                    for component in rel.components() {
                        let part = component.as_os_str().to_string_lossy();
                        if part.is_empty() {
                            continue;
                        }
                        if !first {
                            out.push(':');
                        }
                        first = false;
                        out.push_str(&part);
                    }
                    out
                };

                let cp = manager
                    .classpath_gradle(
                        &project_root,
                        if project_path == ":" {
                            None
                        } else {
                            Some(project_path.as_str())
                        },
                    )
                    .map_err(map_build_error)?;

                let source_roots = config
                    .source_roots
                    .iter()
                    .filter(|root| root.path.starts_with(&module.root))
                    .map(|root| root.path.to_string_lossy().to_string())
                    .collect();

                Ok(ProjectModelUnit::Gradle {
                    project_path,
                    compile_classpath: cp
                        .entries
                        .into_iter()
                        .map(|p| p.to_string_lossy().to_string())
                        .collect(),
                    module_path: Vec::new(),
                    source_roots,
                    language_level: language_level.clone(),
                })
            })
            .collect::<Result<Vec<_>>>()?,
        nova_project::BuildSystem::Simple => {
            let source_roots = config
                .source_roots
                .iter()
                .map(|root| root.path.to_string_lossy().to_string())
                .collect();
            vec![ProjectModelUnit::Simple {
                module: ".".to_string(),
                compile_classpath: config
                    .classpath
                    .iter()
                    .map(|entry| entry.path.to_string_lossy().to_string())
                    .collect(),
                module_path: config
                    .module_path
                    .iter()
                    .map(|entry| entry.path.to_string_lossy().to_string())
                    .collect(),
                source_roots,
                language_level,
            }]
        }
        nova_project::BuildSystem::Bazel => {
            return Err(NovaLspError::InvalidParams(
                "Bazel workspace was not detected at the requested root".to_string(),
            ));
        }
    };

    let result = ProjectModelResult {
        project_root: project_root.to_string_lossy().to_string(),
        units,
    };
    serde_json::to_value(result).map_err(|err| NovaLspError::Internal(err.to_string()))
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
    #[serde(default)]
    pub source: Option<String>,
}

pub fn handle_build_diagnostics(params: serde_json::Value) -> Result<serde_json::Value> {
    let req: BuildDiagnosticsParams = serde_json::from_value(params)
        .map_err(|err| NovaLspError::InvalidParams(err.to_string()))?;

    let requested_root = PathBuf::from(&req.project_root);
    let requested_root = requested_root
        .canonicalize()
        .unwrap_or_else(|_| requested_root.clone());

    if let Some(workspace_root) = nova_project::bazel_workspace_root(&requested_root) {
        let Some(target) = req.target.clone() else {
            // Clients may call this endpoint without selecting a target first. For Bazel workspaces
            // we currently require an explicit target so we can ask the BSP server to compile it.
            return serde_json::to_value(BuildDiagnosticsResult {
                target: None,
                diagnostics: Vec::new(),
                source: Some("bazel: missing `target` (Bazel label)".to_string()),
            })
            .map_err(|err| NovaLspError::Internal(err.to_string()));
        };

        // BSP configuration discovery (env-based).
        //
        // - NOVA_BSP_PROGRAM: launcher executable (e.g. `bsp4bazel`)
        // - NOVA_BSP_ARGS: optional args, either:
        //     - JSON array (e.g. `["--arg1","--arg2"]`)
        //     - whitespace-separated string (quotes are not interpreted)
        let Some(program) = env::var("NOVA_BSP_PROGRAM")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
        else {
            return serde_json::to_value(BuildDiagnosticsResult {
                target: Some(target),
                diagnostics: Vec::new(),
                source: Some("bazel: BSP not configured (set NOVA_BSP_PROGRAM)".to_string()),
            })
            .map_err(|err| NovaLspError::Internal(err.to_string()));
        };

        let args = match env::var("NOVA_BSP_ARGS") {
            Ok(raw) => {
                let raw = raw.trim();
                if raw.is_empty() {
                    Vec::new()
                } else if raw.starts_with('[') {
                    match serde_json::from_str::<Vec<String>>(raw) {
                        Ok(args) => args,
                        Err(err) => {
                            return serde_json::to_value(BuildDiagnosticsResult {
                                target: Some(target),
                                diagnostics: Vec::new(),
                                source: Some(format!(
                                    "bazel: invalid NOVA_BSP_ARGS JSON array: {err}"
                                )),
                            })
                            .map_err(|err| NovaLspError::Internal(err.to_string()));
                        }
                    }
                } else {
                    raw.split_whitespace().map(|s| s.to_string()).collect()
                }
            }
            Err(env::VarError::NotPresent) => Vec::new(),
            Err(err) => {
                return Err(NovaLspError::Internal(format!(
                    "failed to read NOVA_BSP_ARGS: {err}"
                )));
            }
        };

        let config = nova_build_bazel::BazelBspConfig { program, args };
        let targets = vec![target.clone()];
        let publish = match nova_build_bazel::bsp_compile_and_collect_diagnostics(
            &config,
            &workspace_root,
            &targets,
        ) {
            Ok(params) => params,
            Err(err) => {
                return serde_json::to_value(BuildDiagnosticsResult {
                    target: Some(target),
                    diagnostics: Vec::new(),
                    source: Some(format!("bazel: BSP compile failed: {err}")),
                })
                .map_err(|err| NovaLspError::Internal(err.to_string()));
            }
        };

        let diagnostics = nova_build_bazel::bsp_publish_diagnostics_to_nova_diagnostics(&publish)
            .into_iter()
            .map(NovaDiagnostic::from)
            .collect();

        return serde_json::to_value(BuildDiagnosticsResult {
            target: Some(target),
            diagnostics,
            source: Some("bsp".to_string()),
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
    let manager = super::build_manager_for_root(&requested_root, Duration::from_secs(120));
    let result = run_build(&manager, &params)?;
    let resp = BuildDiagnosticsResult {
        target: req.target,
        diagnostics: result
            .diagnostics
            .into_iter()
            .map(NovaDiagnostic::from)
            .collect(),
        source: None,
    };
    serde_json::to_value(resp).map_err(|err| NovaLspError::Internal(err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};
    #[cfg(unix)]
    use std::{fs, os::unix::fs::PermissionsExt};
    use tempfile::TempDir;

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

    #[test]
    fn normalize_maven_module_relative_treats_dot_as_workspace_root() {
        assert_eq!(normalize_maven_module_relative(None), None);
        assert_eq!(normalize_maven_module_relative(Some("")), None);
        assert_eq!(normalize_maven_module_relative(Some("   ")), None);
        assert_eq!(normalize_maven_module_relative(Some(".")), None);
        assert_eq!(
            normalize_maven_module_relative(Some("module-a")),
            Some(Path::new("module-a"))
        );
        assert_eq!(
            normalize_maven_module_relative(Some(" module-b ")),
            Some(Path::new("module-b"))
        );
    }

    #[test]
    fn parse_java_major_accepts_common_formats() {
        assert_eq!(parse_java_major("17"), Some(17));
        assert_eq!(parse_java_major("1.8"), Some(8));
        assert_eq!(parse_java_major("17.0.1"), Some(17));
        assert_eq!(parse_java_major(""), None);
        assert_eq!(parse_java_major("   "), None);
        assert_eq!(parse_java_major("foo"), None);
    }

    #[test]
    fn language_level_from_java_compile_config_prefers_release_then_source_then_target() {
        let cfg = JavaCompileConfig {
            release: Some("21".into()),
            source: Some("17".into()),
            target: Some("11".into()),
            enable_preview: true,
            ..JavaCompileConfig::default()
        };
        assert_eq!(
            language_level_from_java_compile_config(&cfg),
            Some(LanguageLevel {
                major: 21,
                preview: true
            })
        );

        let cfg = JavaCompileConfig {
            release: None,
            source: Some("1.8".into()),
            target: Some("11".into()),
            enable_preview: false,
            ..JavaCompileConfig::default()
        };
        assert_eq!(
            language_level_from_java_compile_config(&cfg),
            Some(LanguageLevel {
                major: 8,
                preview: false
            })
        );
    }

    #[test]
    fn classpath_response_is_backwards_compatible() {
        let resp = NovaClasspathResponse {
            classpath: vec!["/tmp/classes".to_string()],
            module_path: Vec::new(),
            source_roots: Vec::new(),
            generated_source_roots: Vec::new(),
            language_level: LanguageLevel::default(),
            output_dirs: OutputDirs::default(),
        };

        let value = serde_json::to_value(resp).unwrap();
        assert_eq!(
            value
                .get("classpath")
                .and_then(|v| v.as_array())
                .unwrap()
                .len(),
            1
        );
        assert!(value.get("modulePath").is_some());
        assert!(value.get("sourceRoots").is_some());
        assert!(value.get("generatedSourceRoots").is_some());
        assert!(value.get("languageLevel").is_some());
        assert!(value.get("outputDirs").is_some());
    }

    #[test]
    fn target_classpath_requires_target_for_bazel_projects() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("WORKSPACE"), "").unwrap();

        let err = handle_target_classpath(serde_json::json!({
            "projectRoot": tmp.path().to_string_lossy(),
        }))
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("`target` must be provided for Bazel projects"));
    }

    #[test]
    fn project_model_params_accepts_project_root_aliases() {
        let params: ProjectModelParams = serde_json::from_value(serde_json::json!({
            "root": "/tmp/project",
        }))
        .unwrap();

        assert_eq!(params.project_root, "/tmp/project");
    }

    #[test]
    fn project_model_result_roundtrips_through_json() {
        let result = ProjectModelResult {
            project_root: "/workspace".into(),
            units: vec![
                ProjectModelUnit::Maven {
                    module: ".".into(),
                    compile_classpath: vec!["/workspace/target/classes".into()],
                    module_path: vec![],
                    source_roots: vec!["/workspace/src/main/java".into()],
                    language_level: Some(JavaLanguageLevel {
                        source: Some("17".into()),
                        target: Some("17".into()),
                        release: None,
                    }),
                },
                ProjectModelUnit::Gradle {
                    project_path: ":app".into(),
                    compile_classpath: vec!["/workspace/app/build/classes/java/main".into()],
                    module_path: vec![],
                    source_roots: vec!["/workspace/app/src/main/java".into()],
                    language_level: Some(JavaLanguageLevel {
                        source: Some("17".into()),
                        target: Some("17".into()),
                        release: Some("17".into()),
                    }),
                },
                ProjectModelUnit::Bazel {
                    target: "//java/com/example:lib".into(),
                    compile_classpath: vec!["/workspace/bazel-out/lib.jar".into()],
                    module_path: vec!["/workspace/bazel-out/module.jar".into()],
                    source_roots: vec!["/workspace/java/com/example".into()],
                    language_level: Some(JavaLanguageLevel {
                        source: Some("17".into()),
                        target: Some("17".into()),
                        release: None,
                    }),
                },
            ],
        };

        let value = serde_json::to_value(&result).unwrap();
        let decoded: ProjectModelResult = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, result);
    }

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[cfg(unix)]
    fn write_executable(path: &Path, contents: &str) {
        fs::write(path, contents).unwrap();
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn target_classpath_uses_build_manager_for_maven() {
        let _guard = env_lock().lock().unwrap();
        let original_path = std::env::var("PATH").unwrap_or_default();

        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        // Minimal Maven marker.
        fs::write(root.join("pom.xml"), "<project></project>").unwrap();

        // Mock `mvn` executable returning a stable classpath entry.
        let bin_dir = root.join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let fake_jar = root.join("fake-maven.jar");
        let fake_jar_str = fake_jar.to_string_lossy().to_string();
        write_executable(
            &bin_dir.join("mvn"),
            &format!("#!/bin/sh\n\necho {}\n", &fake_jar_str),
        );

        std::env::set_var("PATH", format!("{}:{}", bin_dir.display(), original_path));

        let resp = handle_target_classpath(serde_json::json!({
            "projectRoot": root.to_string_lossy().to_string(),
        }))
        .unwrap();

        std::env::set_var("PATH", original_path);

        let result: TargetClasspathResult = serde_json::from_value(resp).unwrap();
        assert!(
            result.classpath.iter().any(|p| p == &fake_jar_str),
            "classpath should include entry from mocked `mvn`"
        );
    }

    #[test]
    #[cfg(unix)]
    fn target_classpath_uses_build_manager_for_gradle() {
        let _guard = env_lock().lock().unwrap();
        let original_path = std::env::var("PATH").unwrap_or_default();

        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        // Minimal Gradle markers.
        fs::write(root.join("build.gradle"), "plugins {}").unwrap();

        // Mock `gradle` executable returning a stable classpath entry.
        let bin_dir = root.join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let fake_jar = root.join("fake-gradle.jar");
        let fake_jar_str = fake_jar.to_string_lossy().to_string();
        write_executable(
            &bin_dir.join("gradle"),
            &format!("#!/bin/sh\n\necho {}\n", &fake_jar_str),
        );

        std::env::set_var("PATH", format!("{}:{}", bin_dir.display(), original_path));

        let resp = handle_target_classpath(serde_json::json!({
            "projectRoot": root.to_string_lossy().to_string(),
        }))
        .unwrap();

        std::env::set_var("PATH", original_path);

        let result: TargetClasspathResult = serde_json::from_value(resp).unwrap();
        assert!(
            result.classpath.iter().any(|p| p == &fake_jar_str),
            "classpath should include entry from mocked `gradle`"
        );
    }
}
