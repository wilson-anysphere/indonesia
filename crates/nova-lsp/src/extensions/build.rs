use crate::{NovaLspError, Result};
use nova_build::{
    BuildDiagnosticsSnapshot, BuildError, BuildManager, BuildOrchestrator, BuildRequest,
    BuildTaskState, Classpath, CommandOutput, CommandRunner, CommandRunnerFactory,
    DefaultCommandRunner, JavaCompileConfig,
};
use nova_build_bazel::{
    BazelBspConfig, BazelBuildDiagnosticsSnapshot, BazelBuildExecutor, BazelBuildOrchestrator,
    BazelBuildRequest, BspCompileOutcome, DefaultBazelBuildExecutor,
};
use nova_cache::{CacheConfig, CacheDir};
use nova_project::{load_project_with_options, load_workspace_model_with_options, LoadOptions};
use serde::Deserialize;
use serde_json::Value;
use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::Duration,
};

use super::config::{load_workspace_config, load_workspace_config_with_path};

fn build_orchestrators() -> &'static Mutex<HashMap<PathBuf, BuildOrchestrator>> {
    static ORCHESTRATORS: OnceLock<Mutex<HashMap<PathBuf, BuildOrchestrator>>> = OnceLock::new();
    ORCHESTRATORS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn bazel_build_orchestrators() -> &'static Mutex<HashMap<PathBuf, BazelBuildOrchestrator>> {
    static ORCHESTRATORS: OnceLock<Mutex<HashMap<PathBuf, BazelBuildOrchestrator>>> =
        OnceLock::new();
    ORCHESTRATORS.get_or_init(|| Mutex::new(HashMap::new()))
}

type CachedBazelWorkspace =
    Arc<Mutex<nova_build_bazel::BazelWorkspace<nova_build_bazel::DefaultCommandRunner>>>;

fn canonicalize_best_effort(path: &Path, context: &'static str) -> PathBuf {
    match path.canonicalize() {
        Ok(path) => path,
        Err(err) => {
            tracing::debug!(
                target = "nova.lsp",
                context,
                path = %path.display(),
                error = ?err,
                "failed to canonicalize path; using provided path"
            );
            path.to_path_buf()
        }
    }
}

fn cached_bazel_workspaces() -> &'static Mutex<HashMap<PathBuf, CachedBazelWorkspace>> {
    static WORKSPACES: OnceLock<Mutex<HashMap<PathBuf, CachedBazelWorkspace>>> = OnceLock::new();
    WORKSPACES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cached_bazel_workspace_for_root(workspace_root: &Path) -> Result<CachedBazelWorkspace> {
    let canonical = canonicalize_best_effort(workspace_root, "cached_bazel_workspace_for_root");

    {
        let map = crate::poison::lock(cached_bazel_workspaces(), "cached_bazel_workspace_for_root");
        if let Some(existing) = map.get(&canonical) {
            return Ok(Arc::clone(existing));
        }
    }

    let cache_path = CacheDir::new(&canonical, CacheConfig::from_env())
        .map(|dir| dir.queries_dir().join("bazel.json"))
        .map_err(|err| NovaLspError::Internal(err.to_string()))?;
    let runner = nova_build_bazel::DefaultCommandRunner::default();
    let workspace = nova_build_bazel::BazelWorkspace::new(canonical.clone(), runner)
        .and_then(|ws| ws.with_cache_path(cache_path))
        .map_err(|err| NovaLspError::Internal(err.to_string()))?;
    let workspace = Arc::new(Mutex::new(workspace));

    let mut map = crate::poison::lock(cached_bazel_workspaces(), "cached_bazel_workspace_for_root");
    let entry = map
        .entry(canonical)
        .or_insert_with(|| Arc::clone(&workspace));
    Ok(Arc::clone(entry))
}

fn reset_cached_bazel_workspace(workspace_root: &Path) {
    let canonical = canonicalize_best_effort(workspace_root, "reset_cached_bazel_workspace");
    let mut map = crate::poison::lock(cached_bazel_workspaces(), "reset_cached_bazel_workspace");
    map.remove(&canonical);
}

pub fn invalidate_bazel_workspaces(changed: &[PathBuf]) {
    // Most `workspace/didChangeWatchedFiles` notifications are for Java sources, which do not
    // influence Bazel query/aquery evaluation or owning-target resolution. Avoid invalidating Bazel
    // caches for `.java` edits to reduce churn and unnecessary disk writes.
    let mut changed_filtered = Vec::with_capacity(changed.len());
    for path in changed {
        let is_java = path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("java"));
        if is_java {
            continue;
        }
        changed_filtered.push(path.clone());
    }
    if changed_filtered.is_empty() {
        return;
    }

    let workspaces: Vec<CachedBazelWorkspace> = {
        let map = crate::poison::lock(cached_bazel_workspaces(), "invalidate_bazel_workspaces");
        map.values().cloned().collect()
    };

    for workspace in workspaces {
        let mut guard = crate::poison::lock(&workspace, "invalidate_bazel_workspaces/workspace");
        // Best-effort: cache invalidation should never crash the server.
        if let Err(err) = guard.invalidate_changed_files(&changed_filtered) {
            let sample = changed_filtered
                .first()
                .map(|path| path.display().to_string());
            tracing::debug!(
                target = "nova.lsp",
                changed_files = changed_filtered.len(),
                sample = ?sample,
                error = %err,
                "failed to invalidate bazel workspace caches"
            );
        }
    }
}

fn build_orchestrator_if_present(project_root: &Path) -> Option<BuildOrchestrator> {
    let canonical = canonicalize_best_effort(project_root, "build_orchestrator_if_present");
    let map = crate::poison::lock(build_orchestrators(), "build_orchestrator_if_present");
    map.get(&canonical).cloned()
}

fn bazel_build_orchestrator_if_present(workspace_root: &Path) -> Option<BazelBuildOrchestrator> {
    let canonical = canonicalize_best_effort(workspace_root, "bazel_build_orchestrator_if_present");
    let map = crate::poison::lock(
        bazel_build_orchestrators(),
        "bazel_build_orchestrator_if_present",
    );
    map.get(&canonical).cloned()
}

fn build_orchestrator_for_root(project_root: &Path) -> BuildOrchestrator {
    let canonical = canonicalize_best_effort(project_root, "build_orchestrator_for_root");

    {
        let map = crate::poison::lock(build_orchestrators(), "build_orchestrator_for_root/read");
        if let Some(existing) = map.get(&canonical) {
            return existing.clone();
        }
    }

    let cache_dir = match CacheDir::new(&canonical, CacheConfig::from_env()) {
        Ok(dir) => dir.root().join("build"),
        Err(err) => {
            tracing::debug!(
                target = "nova.lsp",
                project_root = %canonical.display(),
                error = %err,
                "failed to determine cache dir; using .nova/build-cache"
            );
            canonical.join(".nova").join("build-cache")
        }
    };
    let runner_factory = Arc::new(BuildStatusCommandRunnerFactory {
        project_root: canonical.clone(),
        timeout: Some(Duration::from_secs(15 * 60)),
    });
    let orchestrator =
        BuildOrchestrator::with_runner_factory(canonical.clone(), cache_dir, runner_factory);

    let mut map = crate::poison::lock(build_orchestrators(), "build_orchestrator_for_root/write");
    map.entry(canonical).or_insert_with(|| orchestrator.clone());
    orchestrator
}

fn bazel_build_orchestrator_for_root(workspace_root: &Path) -> BazelBuildOrchestrator {
    let canonical = canonicalize_best_effort(workspace_root, "bazel_build_orchestrator_for_root");

    {
        let map = crate::poison::lock(
            bazel_build_orchestrators(),
            "bazel_build_orchestrator_for_root/read",
        );
        if let Some(existing) = map.get(&canonical) {
            return existing.clone();
        }
    }

    let executor = Arc::new(BuildStatusBazelBuildExecutor {
        workspace_root: canonical.clone(),
        inner: Arc::new(DefaultBazelBuildExecutor),
    });
    let orchestrator = BazelBuildOrchestrator::with_executor(canonical.clone(), executor);

    let mut map = crate::poison::lock(
        bazel_build_orchestrators(),
        "bazel_build_orchestrator_for_root/write",
    );
    map.entry(canonical).or_insert_with(|| orchestrator.clone());
    orchestrator
}

fn reset_build_orchestrator(project_root: &Path) {
    if let Some(orchestrator) = build_orchestrator_if_present(project_root) {
        orchestrator.reset();
    }
}

fn reset_bazel_build_orchestrator(workspace_root: &Path) {
    if let Some(orchestrator) = bazel_build_orchestrator_if_present(workspace_root) {
        orchestrator.reset();
    }
}

#[derive(Debug, Clone)]
struct ProjectParams {
    project_root: String,
    build_tool: Option<BuildTool>,
    module: Option<String>,
    project_path: Option<String>,
    target: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BuildTool {
    Auto,
    Maven,
    Gradle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LanguageLevel {
    pub major: u16,
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

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct OutputDirs {
    pub main: Vec<String>,
    pub test: Vec<String>,
}

pub const BUILD_PROJECT_SCHEMA_VERSION: u32 = 1;

fn string_array_value(values: Vec<String>) -> Value {
    Value::Array(values.into_iter().map(Value::String).collect())
}

fn opt_string_value(value: Option<String>) -> Value {
    match value {
        Some(value) => Value::String(value),
        None => Value::Null,
    }
}

fn opt_u64_value(value: Option<u64>) -> Value {
    match value {
        Some(value) => Value::from(value),
        None => Value::Null,
    }
}

fn build_task_state_string(state: BuildTaskState) -> &'static str {
    match state {
        BuildTaskState::Idle => "idle",
        BuildTaskState::Queued => "queued",
        BuildTaskState::Running => "running",
        BuildTaskState::Success => "success",
        BuildTaskState::Failure => "failure",
        BuildTaskState::Cancelled => "cancelled",
    }
}

fn build_diagnostic_severity_string(severity: nova_core::BuildDiagnosticSeverity) -> &'static str {
    match severity {
        nova_core::BuildDiagnosticSeverity::Error => "error",
        nova_core::BuildDiagnosticSeverity::Warning => "warning",
        nova_core::BuildDiagnosticSeverity::Information => "information",
        nova_core::BuildDiagnosticSeverity::Hint => "hint",
    }
}

pub(crate) fn build_diagnostic_value(value: nova_core::BuildDiagnostic) -> Value {
    Value::Object({
        let mut diag = serde_json::Map::new();
        diag.insert(
            "file".to_string(),
            Value::String(value.file.to_string_lossy().to_string()),
        );
        diag.insert(
            "range".to_string(),
            Value::Object({
                let mut range = serde_json::Map::new();
                range.insert(
                    "start".to_string(),
                    Value::Object({
                        let mut start = serde_json::Map::new();
                        start.insert("line".to_string(), Value::from(value.range.start.line));
                        start.insert(
                            "character".to_string(),
                            Value::from(value.range.start.character),
                        );
                        start
                    }),
                );
                range.insert(
                    "end".to_string(),
                    Value::Object({
                        let mut end = serde_json::Map::new();
                        end.insert("line".to_string(), Value::from(value.range.end.line));
                        end.insert(
                            "character".to_string(),
                            Value::from(value.range.end.character),
                        );
                        end
                    }),
                );
                range
            }),
        );
        diag.insert(
            "severity".to_string(),
            Value::String(build_diagnostic_severity_string(value.severity).to_string()),
        );
        diag.insert("message".to_string(), Value::String(value.message));
        diag.insert("source".to_string(), opt_string_value(value.source));
        diag
    })
}

fn language_level_value(level: LanguageLevel) -> Value {
    Value::Object({
        let mut value = serde_json::Map::new();
        value.insert("major".to_string(), Value::from(level.major as u64));
        value.insert("preview".to_string(), Value::Bool(level.preview));
        value
    })
}

fn output_dirs_value(dirs: OutputDirs) -> Value {
    Value::Object({
        let mut value = serde_json::Map::new();
        value.insert("main".to_string(), string_array_value(dirs.main));
        value.insert("test".to_string(), string_array_value(dirs.test));
        value
    })
}

fn build_project_response_value(
    build_id: u64,
    status: BuildTaskState,
    diagnostics: Vec<nova_core::BuildDiagnostic>,
) -> Value {
    Value::Object({
        let mut value = serde_json::Map::new();
        value.insert(
            "schemaVersion".to_string(),
            Value::from(u64::from(BUILD_PROJECT_SCHEMA_VERSION)),
        );
        value.insert("buildId".to_string(), Value::from(build_id));
        value.insert(
            "status".to_string(),
            Value::String(build_task_state_string(status).to_string()),
        );
        value.insert(
            "diagnostics".to_string(),
            Value::Array(
                diagnostics
                    .into_iter()
                    .map(build_diagnostic_value)
                    .collect(),
            ),
        );
        value
    })
}

pub fn handle_build_project(params: serde_json::Value) -> Result<serde_json::Value> {
    let params = parse_params(params)?;
    if params.project_root.trim().is_empty() {
        return Err(NovaLspError::InvalidParams(
            "`projectRoot` must not be empty".to_string(),
        ));
    }

    let requested_root = PathBuf::from(&params.project_root);
    let project_root = canonicalize_best_effort(requested_root.as_path(), "handle_build_project");

    let allow_bazel = matches!(params.build_tool, None | Some(BuildTool::Auto));
    let bazel_workspace_root = allow_bazel
        .then(|| nova_project::bazel_workspace_root(&project_root))
        .flatten();
    let bazel_target = params
        .target
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string);

    if let (Some(workspace_root), Some(target)) = (&bazel_workspace_root, &bazel_target) {
        let orchestrator = bazel_build_orchestrator_for_root(workspace_root);
        let build_id = orchestrator.enqueue(BazelBuildRequest {
            targets: vec![target.clone()],
            // BSP config resolution is handled inside `nova-build-bazel`:
            // - standard `.bsp/*.json` discovery
            // - `NOVA_BSP_PROGRAM` / `NOVA_BSP_ARGS` overrides
            bsp_config: None,
        });

        let status = orchestrator.status();
        let diagnostics = orchestrator.diagnostics().diagnostics;

        return Ok(build_project_response_value(
            build_id,
            status.state,
            diagnostics,
        ));
    }

    let kind = match detect_kind(&project_root, params.build_tool) {
        Ok(kind) => kind,
        Err(err)
            if bazel_workspace_root.is_some()
                && bazel_target.is_none()
                && matches!(params.build_tool, None | Some(BuildTool::Auto)) =>
        {
            return Err(NovaLspError::InvalidParams(
                "`target` must be provided for Bazel projects".to_string(),
            ));
        }
        Err(err) => return Err(err),
    };
    let request = match kind {
        BuildKind::Maven => BuildRequest::Maven {
            module_relative: normalize_maven_module_relative(params.module.as_deref())
                .map(|p| p.to_path_buf()),
            goal: nova_build::MavenBuildGoal::Compile,
        },
        BuildKind::Gradle => BuildRequest::Gradle {
            project_path: normalize_gradle_project_path(params.project_path.as_deref())
                .map(|p| p.into_owned()),
            task: nova_build::GradleBuildTask::CompileJava,
        },
    };

    let orchestrator = build_orchestrator_for_root(&project_root);
    let build_id = orchestrator.enqueue(request);
    let status = orchestrator.status();
    let diagnostics = orchestrator.diagnostics().diagnostics;
    Ok(build_project_response_value(
        build_id,
        status.state,
        diagnostics,
    ))
}

pub fn handle_java_classpath(params: serde_json::Value) -> Result<serde_json::Value> {
    static COMPILE_CONFIG_FALLBACK_LOGS: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(0);

    let params = parse_params(params)?;
    let project_root = PathBuf::from(&params.project_root);
    let manager = super::build_manager_for_root(&project_root, Duration::from_secs(60));
    let metadata = load_build_metadata(&params);

    let mut status_guard = BuildStatusGuard::new(&project_root);
    let classpath_result: Result<(
        Vec<String>,
        Vec<String>,
        Vec<String>,
        LanguageLevel,
        OutputDirs,
    )> = (|| {
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

        Ok(match compile_config {
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
                let language_level = language_level_from_java_compile_config(&cfg)
                    .unwrap_or(metadata.language_level);
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
            Err(err) => {
                if COMPILE_CONFIG_FALLBACK_LOGS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    < 10
                {
                    tracing::debug!(
                        target = "nova.lsp",
                        project_root = %project_root.display(),
                        build_kind = ?kind,
                        error = %err,
                        "failed to extract java compile config; falling back to legacy classpath"
                    );
                }
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
        })
    })();
    status_guard.finish_from_result(&classpath_result);
    let (classpath, module_path, source_roots, language_level, output_dirs) = classpath_result?;

    Ok(Value::Object({
        let mut resp = serde_json::Map::new();
        resp.insert("classpath".to_string(), string_array_value(classpath));
        resp.insert("modulePath".to_string(), string_array_value(module_path));
        resp.insert("sourceRoots".to_string(), string_array_value(source_roots));
        resp.insert(
            "generatedSourceRoots".to_string(),
            string_array_value(metadata.generated_source_roots),
        );
        resp.insert(
            "languageLevel".to_string(),
            language_level_value(language_level),
        );
        resp.insert("outputDirs".to_string(), output_dirs_value(output_dirs));
        resp
    }))
}

pub fn handle_reload_project(params: serde_json::Value) -> Result<serde_json::Value> {
    let params = parse_params(params)?;
    if params.project_root.trim().is_empty() {
        return Err(NovaLspError::InvalidParams(
            "`projectRoot` must not be empty".to_string(),
        ));
    }

    let requested_root = PathBuf::from(&params.project_root);
    let project_root = canonicalize_best_effort(requested_root.as_path(), "handle_reload_project");

    reset_build_orchestrator(&project_root);
    if let Some(workspace_root) = nova_project::bazel_workspace_root(&project_root) {
        reset_bazel_build_orchestrator(&workspace_root);
        reset_cached_bazel_workspace(&workspace_root);

        match CacheDir::new(&workspace_root, CacheConfig::from_env()) {
            Ok(cache_dir) => {
                let cache_path = cache_dir.queries_dir().join("bazel.json");
                match std::fs::remove_file(&cache_path) {
                    Ok(()) => {}
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => {
                        tracing::debug!(
                            target = "nova.lsp",
                            path = %cache_path.display(),
                            error = %err,
                            "failed to evict bazel cache file"
                        );
                    }
                }
            }
            Err(err) => {
                tracing::debug!(
                    target = "nova.lsp",
                    workspace_root = %workspace_root.display(),
                    error = %err,
                    "failed to open cache dir; skipping bazel cache eviction"
                );
            }
        }
    }

    let manager = super::build_manager_for_root(&project_root, Duration::from_secs(60));
    manager
        .reload_project(&project_root)
        .map_err(map_build_error)?;
    Ok(serde_json::Value::Null)
}

fn parse_params(params: serde_json::Value) -> Result<ProjectParams> {
    let obj = params
        .as_object()
        .ok_or_else(|| NovaLspError::InvalidParams("params must be an object".to_string()))?;
    let project_root = super::get_str(obj, &["projectRoot", "project_root", "root"])
        .ok_or_else(|| NovaLspError::InvalidParams("missing required `projectRoot`".to_string()))?
        .to_string();

    let build_tool = match super::get_str(obj, &["buildTool", "build_tool", "kind"]) {
        None => None,
        Some(tool) => {
            let tool = tool.trim();
            if tool.is_empty() {
                None
            } else {
                Some(match tool.to_ascii_lowercase().as_str() {
                    "auto" => BuildTool::Auto,
                    "maven" => BuildTool::Maven,
                    "gradle" => BuildTool::Gradle,
                    _ => {
                        return Err(NovaLspError::InvalidParams(format!(
                            "invalid build tool `{tool}`"
                        )));
                    }
                })
            }
        }
    };

    let module = super::get_str(obj, &["module"]).map(str::to_string);
    let project_path = super::get_str(obj, &["projectPath", "project_path"]).map(str::to_string);
    let target = super::get_str(obj, &["target"]).map(str::to_string);

    Ok(ProjectParams {
        project_root,
        build_tool,
        module,
        project_path,
        target,
    })
}

fn run_classpath(build: &BuildManager, params: &ProjectParams) -> Result<Classpath> {
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

#[derive(Debug)]
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

fn composite_gradle_build_root_project_path(project_path: &str) -> Option<&str> {
    // Keep logic aligned with `nova-project`'s Gradle composite build modeling.
    //
    // Gradle's `buildSrc` and `includeBuild(...)` are separate builds, but `nova-project` exposes
    // them as synthetic project paths so they can participate in a workspace-wide module model:
    // - buildSrc root: `:__buildSrc`
    // - buildSrc subprojects: `:__buildSrc:subproject`
    // - included build root: `:__includedBuild_<name>`
    // - included build subprojects: `:__includedBuild_<name>:subproject`
    //
    // When invoking Gradle tasks we must call Gradle *within the composite build root* and use the
    // inner Gradle project path (e.g. `:subproject`), not the synthetic prefix.
    const BUILDSRC_PREFIX: &str = ":__buildSrc";

    if let Some(rest) = project_path.strip_prefix(BUILDSRC_PREFIX) {
        if rest.is_empty() || rest.starts_with(':') {
            return Some(BUILDSRC_PREFIX);
        }
    }

    if !project_path.starts_with(":__includedBuild_") {
        return None;
    }

    let rest = project_path.strip_prefix(':').unwrap_or(project_path);
    match rest.find(':') {
        Some(idx) => Some(&project_path[..idx + 1]),
        None => Some(project_path),
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
    match digits.parse::<u16>() {
        Ok(value) => Some(value),
        Err(err) => {
            tracing::debug!(
                target = "nova.lsp",
                input = %trimmed,
                digits_len = digits.len(),
                error = %err,
                "failed to parse java major version"
            );
            None
        }
    }
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

fn load_build_metadata(params: &ProjectParams) -> BuildMetadata {
    let root = PathBuf::from(&params.project_root);
    let kind = match detect_kind(&root, params.build_tool) {
        Ok(kind) => kind,
        Err(err) => {
            tracing::debug!(
                target = "nova.lsp",
                project_root = %root.display(),
                error = %err,
                "failed to detect build kind; returning default build metadata"
            );
            return BuildMetadata::default();
        }
    };

    let (nova_config, nova_config_path) = load_workspace_config_with_path(&root);
    let mut options = LoadOptions::default();
    options.nova_config = nova_config;
    options.nova_config_path = nova_config_path;
    let project = match load_project_with_options(&root, &options) {
        Ok(project) => project,
        Err(err) => {
            tracing::debug!(
                target = "nova.lsp",
                project_root = %root.display(),
                error = %err,
                "failed to load project for build metadata; returning defaults"
            );
            return BuildMetadata::default();
        }
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
            if let Some(project_path) = params.project_path.as_deref() {
                if let Some(root) =
                    super::gradle::resolve_gradle_module_root(&project.workspace_root, project_path)
                {
                    vec![root]
                } else {
                    project.modules.iter().map(|m| m.root.clone()).collect()
                }
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

// -----------------------------------------------------------------------------
// Target-aware build metadata (Bazel/BSP)
// -----------------------------------------------------------------------------

fn target_classpath_value(
    project_root: String,
    target: Option<String>,
    classpath: Vec<String>,
    module_path: Vec<String>,
    source_roots: Vec<String>,
    source: Option<String>,
    target_version: Option<String>,
    release: Option<String>,
    output_dir: Option<String>,
    enable_preview: bool,
) -> Value {
    Value::Object({
        let mut value = serde_json::Map::new();
        value.insert("projectRoot".to_string(), Value::String(project_root));
        value.insert("target".to_string(), opt_string_value(target));
        value.insert("classpath".to_string(), string_array_value(classpath));
        value.insert("modulePath".to_string(), string_array_value(module_path));
        value.insert("sourceRoots".to_string(), string_array_value(source_roots));
        value.insert("source".to_string(), opt_string_value(source));
        value.insert(
            "targetVersion".to_string(),
            opt_string_value(target_version),
        );
        value.insert("release".to_string(), opt_string_value(release));
        value.insert("outputDir".to_string(), opt_string_value(output_dir));
        value.insert("enablePreview".to_string(), Value::Bool(enable_preview));
        value
    })
}

pub fn handle_target_classpath(params: serde_json::Value) -> Result<serde_json::Value> {
    let req = parse_params(params)?;

    if req.project_root.trim().is_empty() {
        return Err(NovaLspError::InvalidParams(
            "`projectRoot` must not be empty".to_string(),
        ));
    }

    let requested_root = PathBuf::from(&req.project_root);
    let requested_root =
        canonicalize_best_effort(requested_root.as_path(), "handle_target_classpath");

    if let Some(workspace_root) = nova_project::bazel_workspace_root(&requested_root) {
        let Some(target) = req.target.clone() else {
            return Err(NovaLspError::InvalidParams(
                "`target` must be provided for Bazel projects".to_string(),
            ));
        };
        let mut status_guard = BuildStatusGuard::new(&workspace_root);
        let value_result: Result<serde_json::Value> = (|| {
            let workspace = cached_bazel_workspace_for_root(&workspace_root)?;
            let mut workspace =
                crate::poison::lock(&workspace, "handle_target_classpath/bazel_workspace");

            let info = workspace
                .target_compile_info(&target)
                .map_err(|err| NovaLspError::Internal(err.to_string()))?;

            Ok(target_classpath_value(
                workspace_root.to_string_lossy().to_string(),
                Some(target),
                info.classpath,
                info.module_path,
                info.source_roots,
                info.source,
                info.target,
                info.release,
                info.output_dir,
                info.preview,
            ))
        })();
        status_guard.finish_from_result(&value_result);
        value_result
    } else {
        let (nova_config, nova_config_path) = load_workspace_config_with_path(&requested_root);
        let mut options = LoadOptions::default();
        options.nova_config = nova_config;
        options.nova_config_path = nova_config_path;
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

        let mut status_guard = BuildStatusGuard::new(&project_root);
        let value_result: Result<serde_json::Value> = (|| {
            let (
                classpath,
                module_path,
                source_roots,
                source,
                target_version,
                release,
                output_dir,
                enable_preview,
            ) = match config.build_system {
                nova_project::BuildSystem::Maven => {
                    let module_relative =
                        normalize_maven_module_relative(normalized_target.as_deref());
                    let cfg = manager
                        .java_compile_config_maven(&project_root, module_relative)
                        .map_err(map_build_error)?;
                    let selected_root = module_relative.map(|rel| project_root.join(rel));

                    let JavaCompileConfig {
                        compile_classpath,
                        module_path: cfg_module_path,
                        main_source_roots,
                        test_source_roots,
                        main_output_dir,
                        source: cfg_source,
                        target: cfg_target,
                        release: cfg_release,
                        enable_preview,
                        ..
                    } = cfg;

                    let classpath = paths_to_strings(compile_classpath.iter());
                    let module_path = if cfg_module_path.is_empty() {
                        config
                            .module_path
                            .iter()
                            .map(|entry| entry.path.to_string_lossy().to_string())
                            .collect()
                    } else {
                        paths_to_strings(cfg_module_path.iter())
                    };

                    let mut source_roots: Vec<String> = main_source_roots
                        .iter()
                        .chain(test_source_roots.iter())
                        .map(|p| p.to_string_lossy().to_string())
                        .collect();
                    source_roots.extend(
                        config
                            .source_roots
                            .iter()
                            .filter(|root| {
                                selected_root
                                    .as_ref()
                                    .map_or(true, |selected| root.path.starts_with(selected))
                            })
                            .map(|root| root.path.to_string_lossy().to_string()),
                    );
                    source_roots.sort();
                    source_roots.dedup();

                    let source = cfg_source.or_else(|| Some(config.java.source.0.to_string()));
                    let target_version =
                        cfg_target.or_else(|| Some(config.java.target.0.to_string()));

                    Ok((
                        classpath,
                        module_path,
                        source_roots,
                        source,
                        target_version,
                        cfg_release,
                        main_output_dir.map(|p| p.to_string_lossy().to_string()),
                        enable_preview,
                    ))
                }
                nova_project::BuildSystem::Gradle => {
                    let project_path = normalize_gradle_project_path(normalized_target.as_deref());
                    let cfg = manager
                        .java_compile_config_gradle(&project_root, project_path.as_deref())
                        .map_err(map_build_error)?;
                    let selected_root = project_path.as_deref().and_then(|path| {
                        super::gradle::resolve_gradle_module_root(&project_root, path)
                    });

                    let JavaCompileConfig {
                        compile_classpath,
                        module_path: cfg_module_path,
                        main_source_roots,
                        test_source_roots,
                        main_output_dir,
                        source: cfg_source,
                        target: cfg_target,
                        release: cfg_release,
                        enable_preview,
                        ..
                    } = cfg;

                    let classpath = paths_to_strings(compile_classpath.iter());
                    let module_path = if cfg_module_path.is_empty() {
                        config
                            .module_path
                            .iter()
                            .map(|entry| entry.path.to_string_lossy().to_string())
                            .collect()
                    } else {
                        paths_to_strings(cfg_module_path.iter())
                    };

                    let mut source_roots: Vec<String> = main_source_roots
                        .iter()
                        .chain(test_source_roots.iter())
                        .map(|p| p.to_string_lossy().to_string())
                        .collect();
                    source_roots.extend(
                        config
                            .source_roots
                            .iter()
                            .filter(|root| {
                                selected_root
                                    .as_ref()
                                    .map_or(true, |selected| root.path.starts_with(selected))
                            })
                            .map(|root| root.path.to_string_lossy().to_string()),
                    );
                    source_roots.sort();
                    source_roots.dedup();

                    let source = cfg_source.or_else(|| Some(config.java.source.0.to_string()));
                    let target_version =
                        cfg_target.or_else(|| Some(config.java.target.0.to_string()));

                    Ok((
                        classpath,
                        module_path,
                        source_roots,
                        source,
                        target_version,
                        cfg_release,
                        main_output_dir.map(|p| p.to_string_lossy().to_string()),
                        enable_preview,
                    ))
                }
                // For simple projects, `nova-project` is already the source of truth.
                nova_project::BuildSystem::Simple => Ok((
                    config
                        .classpath
                        .iter()
                        .map(|entry| entry.path.to_string_lossy().to_string())
                        .collect(),
                    config
                        .module_path
                        .iter()
                        .map(|entry| entry.path.to_string_lossy().to_string())
                        .collect(),
                    config
                        .source_roots
                        .iter()
                        .map(|root| root.path.to_string_lossy().to_string())
                        .collect(),
                    Some(config.java.source.0.to_string()),
                    Some(config.java.target.0.to_string()),
                    None,
                    None,
                    false,
                )),
                // Bazel workspaces are handled above via `bazel_workspace_root`.
                nova_project::BuildSystem::Bazel => Err(NovaLspError::InvalidParams(
                    "Bazel workspace was not detected at the requested root".to_string(),
                )),
            }?;

            Ok(target_classpath_value(
                project_root.to_string_lossy().to_string(),
                normalized_target,
                classpath,
                module_path,
                source_roots,
                source,
                target_version,
                release,
                output_dir,
                enable_preview,
            ))
        })();

        status_guard.finish_from_result(&value_result);
        value_result
    }
}

pub fn handle_file_classpath(params: serde_json::Value) -> Result<serde_json::Value> {
    let req = parse_params(params.clone())?;

    if req.project_root.trim().is_empty() {
        return Err(NovaLspError::InvalidParams(
            "`projectRoot` must not be empty".to_string(),
        ));
    }

    let obj = params
        .as_object()
        .ok_or_else(|| NovaLspError::InvalidParams("params must be an object".to_string()))?;
    let Some(uri) = obj.get("uri").and_then(serde_json::Value::as_str) else {
        return Err(NovaLspError::InvalidParams(
            "`uri` must be provided".to_string(),
        ));
    };
    let uri = uri.trim();
    if uri.is_empty() {
        return Err(NovaLspError::InvalidParams(
            "`uri` must be provided".to_string(),
        ));
    }

    let requested_root = PathBuf::from(&req.project_root);
    let requested_root =
        canonicalize_best_effort(requested_root.as_path(), "handle_file_classpath");

    let Some(workspace_root) = nova_project::bazel_workspace_root(&requested_root) else {
        return Err(NovaLspError::InvalidParams(
            "`projectRoot` must be within a Bazel workspace for fileClasspath".to_string(),
        ));
    };

    let url = url::Url::parse(uri)
        .map_err(|err| NovaLspError::InvalidParams(format!("invalid uri: {err}")))?;
    let path = url
        .to_file_path()
        .map_err(|_| NovaLspError::InvalidParams("`uri` must be a file:// URI".to_string()))?;

    let mut status_guard = BuildStatusGuard::new(&workspace_root);
    let value_result: Result<serde_json::Value> = (|| {
        let workspace = cached_bazel_workspace_for_root(&workspace_root)?;
        let mut workspace =
            crate::poison::lock(&workspace, "handle_file_classpath/bazel_workspace");

        let run_target = super::get_str(obj, &["runTarget", "run_target"])
            .map(str::trim)
            .filter(|t| !t.is_empty());
        let info = match run_target {
            Some(run_target) => workspace
                .compile_info_for_file_in_run_target_closure(&path, run_target)
                .map_err(|err| NovaLspError::Internal(err.to_string()))?,
            None => workspace
                .compile_info_for_file(&path)
                .map_err(|err| NovaLspError::Internal(err.to_string()))?,
        };

        let Some(info) = info else {
            return Ok(serde_json::Value::Null);
        };

        Ok(target_classpath_value(
            workspace_root.to_string_lossy().to_string(),
            None,
            info.classpath,
            info.module_path,
            info.source_roots,
            info.source,
            info.target,
            info.release,
            info.output_dir,
            info.preview,
        ))
    })();
    status_guard.finish_from_result(&value_result);
    value_result
}

// -----------------------------------------------------------------------------
// Unified project model (Maven/Gradle/Bazel)
// -----------------------------------------------------------------------------

fn java_language_level_value(
    source: Option<String>,
    target: Option<String>,
    release: Option<String>,
) -> Value {
    Value::Object({
        let mut value = serde_json::Map::new();
        value.insert("source".to_string(), opt_string_value(source));
        value.insert("target".to_string(), opt_string_value(target));
        value.insert("release".to_string(), opt_string_value(release));
        value
    })
}

fn opt_java_language_level_value(
    level: Option<(Option<String>, Option<String>, Option<String>)>,
) -> Value {
    match level {
        Some((source, target, release)) => java_language_level_value(source, target, release),
        None => Value::Null,
    }
}

fn project_model_unit_value(
    kind: &'static str,
    unit_key: &'static str,
    unit_value: String,
    compile_classpath: Vec<String>,
    module_path: Vec<String>,
    source_roots: Vec<String>,
    language_level: Option<(Option<String>, Option<String>, Option<String>)>,
) -> Value {
    Value::Object({
        let mut value = serde_json::Map::new();
        value.insert("kind".to_string(), Value::String(kind.to_string()));
        value.insert(unit_key.to_string(), Value::String(unit_value));
        value.insert(
            "compileClasspath".to_string(),
            string_array_value(compile_classpath),
        );
        value.insert("modulePath".to_string(), string_array_value(module_path));
        value.insert("sourceRoots".to_string(), string_array_value(source_roots));
        value.insert(
            "languageLevel".to_string(),
            opt_java_language_level_value(language_level),
        );
        value
    })
}

fn project_model_result_value(project_root: String, units: Vec<Value>) -> Value {
    Value::Object({
        let mut value = serde_json::Map::new();
        value.insert("projectRoot".to_string(), Value::String(project_root));
        value.insert("units".to_string(), Value::Array(units));
        value
    })
}

pub fn handle_project_model(params: serde_json::Value) -> Result<serde_json::Value> {
    let req = parse_params(params)?;

    if req.project_root.trim().is_empty() {
        return Err(NovaLspError::InvalidParams(
            "`projectRoot` must not be empty".to_string(),
        ));
    }

    let requested_root = PathBuf::from(&req.project_root);
    let requested_root = canonicalize_best_effort(requested_root.as_path(), "handle_project_model");

    if let Some(workspace_root) = nova_project::bazel_workspace_root(&requested_root) {
        let mut status_guard = BuildStatusGuard::new(&workspace_root);
        let value_result: Result<serde_json::Value> = (|| {
            let workspace = cached_bazel_workspace_for_root(&workspace_root)?;
            let mut workspace =
                crate::poison::lock(&workspace, "handle_project_model/bazel_workspace");

            let targets = workspace
                .java_targets()
                .map_err(|err| NovaLspError::Internal(err.to_string()))?;

            let mut units = Vec::with_capacity(targets.len());
            for target in targets {
                let info = workspace
                    .target_compile_info(&target)
                    .map_err(|err| NovaLspError::Internal(err.to_string()))?;
                units.push(project_model_unit_value(
                    "bazel",
                    "target",
                    target,
                    info.classpath,
                    info.module_path,
                    info.source_roots,
                    Some((info.source, info.target, None)),
                ));
            }

            Ok(project_model_result_value(
                workspace_root.to_string_lossy().to_string(),
                units,
            ))
        })();

        status_guard.finish_from_result(&value_result);
        return value_result;
    }

    let nova_config = load_workspace_config(&requested_root);
    let mut options = LoadOptions::default();
    options.nova_config = nova_config;
    let config = load_project_with_options(&requested_root, &options)
        .map_err(|err| NovaLspError::InvalidParams(err.to_string()))?;
    let project_root = config.workspace_root.clone();

    let manager = super::build_manager_for_root(&project_root, Duration::from_secs(120));

    match config.build_system {
        nova_project::BuildSystem::Maven | nova_project::BuildSystem::Gradle => {
            let build_system = config.build_system;
            let mut status_guard = BuildStatusGuard::new(&project_root);
            let value_result: Result<serde_json::Value> = (|| {
                let units = match build_system {
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
                            let cfg = manager
                                .java_compile_config_maven(&project_root, module_relative)
                                .map_err(map_build_error)?;

                            let JavaCompileConfig {
                                compile_classpath,
                                module_path: cfg_module_path,
                                main_source_roots,
                                test_source_roots,
                                source,
                                target,
                                release,
                                ..
                            } = cfg;

                            let mut source_roots: Vec<String> = main_source_roots
                                .iter()
                                .chain(test_source_roots.iter())
                                .map(|root| root.to_string_lossy().to_string())
                                .collect();
                            source_roots.extend(
                                config
                                    .source_roots
                                    .iter()
                                    .filter(|root| root.path.starts_with(&module.root))
                                    .map(|root| root.path.to_string_lossy().to_string()),
                            );
                            source_roots.sort();
                            source_roots.dedup();

                            Ok(project_model_unit_value(
                                "maven",
                                "module",
                                rel,
                                paths_to_strings(compile_classpath.iter()),
                                if cfg_module_path.is_empty() {
                                    config
                                        .module_path
                                        .iter()
                                        .map(|entry| entry.path.to_string_lossy().to_string())
                                        .collect()
                                } else {
                                    paths_to_strings(cfg_module_path.iter())
                                },
                                source_roots,
                                Some((
                                    source.or_else(|| Some(config.java.source.0.to_string())),
                                    target.or_else(|| Some(config.java.target.0.to_string())),
                                    release,
                                )),
                            ))
                        })
                        .collect::<Result<Vec<_>>>()?,
                    nova_project::BuildSystem::Gradle => {
                        let workspace_model =
                            load_workspace_model_with_options(&project_root, &options)
                                .map_err(|err| NovaLspError::InvalidParams(err.to_string()))?;
                        let mut gradle_project_paths_by_root: BTreeMap<PathBuf, String> =
                            BTreeMap::new();
                        let mut gradle_roots_by_project_path: BTreeMap<String, PathBuf> =
                            BTreeMap::new();
                        for module in &workspace_model.modules {
                            let nova_project::WorkspaceModuleBuildId::Gradle { project_path } =
                                &module.build_id
                            else {
                                continue;
                            };
                            gradle_project_paths_by_root
                                .entry(module.root.clone())
                                .or_insert_with(|| project_path.clone());
                            gradle_roots_by_project_path
                                .entry(project_path.clone())
                                .or_insert_with(|| module.root.clone());
                        }

                        // Prefer fetching all Gradle module configs in a single Gradle invocation
                        // for multi-module workspaces. Fall back to per-module queries when the
                        // batch task fails (e.g. older Gradle versions).
                        let mut gradle_configs_by_path =
                            HashMap::<String, JavaCompileConfig>::new();
                        if config.modules.len() > 1 {
                            match manager.java_compile_configs_all_gradle(&project_root) {
                                Ok(configs) => {
                                    gradle_configs_by_path = configs.into_iter().collect();
                                }
                                Err(err) => {
                                    tracing::debug!(
                                        target = "nova.lsp",
                                        project_root = %project_root.display(),
                                        error = %err,
                                        "failed to load Gradle compile configs via batch task; falling back to per-module queries"
                                    );
                                }
                            }
                        }

                        let mut units = Vec::with_capacity(config.modules.len());
                        for module in config.modules.iter() {
                            let module_root = match module.root.canonicalize() {
                                Ok(root) => root,
                                Err(err) => {
                                    tracing::debug!(
                                        target = "nova.lsp",
                                        root = %module.root.display(),
                                        err = %err,
                                        "failed to canonicalize Gradle module root; using raw path"
                                    );
                                    // Use the module root as a best-effort key if canonicalization
                                    // fails (e.g. missing directory).
                                    module.root.clone()
                                }
                            };
                            let project_path = gradle_project_paths_by_root
                                .get(&module_root)
                                .cloned()
                                .ok_or_else(|| {
                                    NovaLspError::Internal(format!(
                                        "failed to resolve Gradle project path for module root {module_root}",
                                        module_root = module_root.display()
                                    ))
                                })?;

                            let composite_root_project_path =
                                composite_gradle_build_root_project_path(&project_path);
                            let is_buildsrc =
                                composite_root_project_path.is_some_and(|root_project_path| {
                                    root_project_path == ":__buildSrc"
                                });

                            // `nova-build` already knows how to invoke Gradle's special `buildSrc`
                            // build by passing `--project-dir buildSrc` when the project path is
                            // `:__buildSrc` (or a nested `:__buildSrc:*` path). Keep the invocation
                            // rooted at the main workspace so we can still use `./gradlew`.
                            let (invocation_root, invocation_project_path) = if is_buildsrc {
                                (project_root.as_path(), Some(project_path.as_str()))
                            } else if let Some(root_project_path) = composite_root_project_path {
                                match gradle_roots_by_project_path.get(root_project_path) {
                                    Some(build_root) => {
                                        let inner = match project_path
                                            .strip_prefix(root_project_path)
                                        {
                                            Some(inner) => inner,
                                            None => {
                                                return Err(NovaLspError::Internal(format!(
                                                    "composite Gradle project path {project_path:?} is missing expected root prefix {root_project_path:?}",
                                                )));
                                            }
                                        };
                                        let inner =
                                            if inner.is_empty() { None } else { Some(inner) };
                                        (build_root.as_path(), inner)
                                    }
                                    None => (project_root.as_path(), Some(project_path.as_str())),
                                }
                            } else {
                                (project_root.as_path(), Some(project_path.as_str()))
                            };

                            let cfg = if invocation_root == project_root.as_path() {
                                if project_path == ":" {
                                    manager.java_compile_config_gradle(&project_root, None)
                                } else if let Some(cfg) =
                                    gradle_configs_by_path.remove(&project_path)
                                {
                                    Ok(cfg)
                                } else {
                                    manager.java_compile_config_gradle(
                                        &project_root,
                                        Some(project_path.as_str()),
                                    )
                                }
                            } else {
                                manager.java_compile_config_gradle(
                                    invocation_root,
                                    invocation_project_path,
                                )
                            }
                            .map_err(map_build_error)?;

                            let JavaCompileConfig {
                                compile_classpath,
                                module_path: cfg_module_path,
                                main_source_roots,
                                test_source_roots,
                                source,
                                target,
                                release,
                                ..
                            } = cfg;

                            let mut source_roots: Vec<String> = main_source_roots
                                .iter()
                                .chain(test_source_roots.iter())
                                .map(|root| root.to_string_lossy().to_string())
                                .collect();
                            source_roots.extend(
                                config
                                    .source_roots
                                    .iter()
                                    .filter(|root| root.path.starts_with(&module.root))
                                    .map(|root| root.path.to_string_lossy().to_string()),
                            );
                            source_roots.sort();
                            source_roots.dedup();

                            units.push(project_model_unit_value(
                                "gradle",
                                "projectPath",
                                project_path,
                                paths_to_strings(compile_classpath.iter()),
                                if cfg_module_path.is_empty() {
                                    config
                                        .module_path
                                        .iter()
                                        .map(|entry| entry.path.to_string_lossy().to_string())
                                        .collect()
                                } else {
                                    paths_to_strings(cfg_module_path.iter())
                                },
                                source_roots,
                                Some((
                                    source.or_else(|| Some(config.java.source.0.to_string())),
                                    target.or_else(|| Some(config.java.target.0.to_string())),
                                    release,
                                )),
                            ));
                        }

                        units
                    }
                    nova_project::BuildSystem::Simple => {
                        return Err(NovaLspError::Internal(
                            "unexpected BuildSystem::Simple while building Gradle project model"
                                .to_string(),
                        ));
                    }
                    nova_project::BuildSystem::Bazel => {
                        return Err(NovaLspError::Internal(
                            "unexpected BuildSystem::Bazel while building Gradle project model"
                                .to_string(),
                        ));
                    }
                };

                Ok(project_model_result_value(
                    project_root.to_string_lossy().to_string(),
                    units,
                ))
            })();

            status_guard.finish_from_result(&value_result);
            value_result
        }
        nova_project::BuildSystem::Simple => {
            let source_roots = config
                .source_roots
                .iter()
                .map(|root| root.path.to_string_lossy().to_string())
                .collect();
            Ok(project_model_result_value(
                project_root.to_string_lossy().to_string(),
                vec![project_model_unit_value(
                    "simple",
                    "module",
                    ".".to_string(),
                    config
                        .classpath
                        .iter()
                        .map(|entry| entry.path.to_string_lossy().to_string())
                        .collect(),
                    config
                        .module_path
                        .iter()
                        .map(|entry| entry.path.to_string_lossy().to_string())
                        .collect(),
                    source_roots,
                    Some((
                        Some(config.java.source.0.to_string()),
                        Some(config.java.target.0.to_string()),
                        None,
                    )),
                )],
            ))
        }
        nova_project::BuildSystem::Bazel => Err(NovaLspError::InvalidParams(
            "Bazel workspace was not detected at the requested root".to_string(),
        )),
    }
}

// -----------------------------------------------------------------------------
// Build status tracking (`nova/build/status`)
// -----------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
struct BuildStatusEntry {
    in_flight_count: u32,
    last_failed: bool,
    last_error: Option<String>,
}

static BUILD_STATUS_REGISTRY: OnceLock<Mutex<BTreeMap<PathBuf, BuildStatusEntry>>> =
    OnceLock::new();

fn build_status_registry() -> &'static Mutex<BTreeMap<PathBuf, BuildStatusEntry>> {
    BUILD_STATUS_REGISTRY.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn canonicalize_project_root(project_root: &Path) -> PathBuf {
    canonicalize_best_effort(project_root, "canonicalize_project_root")
}

fn build_status_snapshot_for_project_root(project_root: &Path) -> (BuildStatus, Option<String>) {
    let key = canonicalize_project_root(project_root);
    let registry = crate::poison::lock(
        build_status_registry(),
        "build_status_snapshot_for_project_root",
    );
    match registry.get(&key) {
        Some(entry) if entry.in_flight_count > 0 => (BuildStatus::Building, None),
        Some(entry) if entry.last_failed => (BuildStatus::Failed, entry.last_error.clone()),
        _ => (BuildStatus::Idle, None),
    }
}

#[cfg(test)]
fn build_status_for_project_root(project_root: &Path) -> BuildStatus {
    build_status_snapshot_for_project_root(project_root).0
}

#[derive(Debug)]
enum BuildInvocationOutcome {
    Success,
    Failure(Option<String>),
}

#[derive(Debug)]
pub(super) struct BuildStatusGuard {
    project_root: PathBuf,
    outcome: Option<BuildInvocationOutcome>,
}

impl BuildStatusGuard {
    pub(super) fn new(project_root: &Path) -> Self {
        let project_root = canonicalize_project_root(project_root);
        let mut registry = crate::poison::lock(build_status_registry(), "BuildStatusGuard::new");
        let entry = registry.entry(project_root.clone()).or_default();
        entry.in_flight_count = entry.in_flight_count.saturating_add(1);
        drop(registry);

        Self {
            project_root,
            outcome: None,
        }
    }

    pub(super) fn mark_success(&mut self) {
        self.outcome = Some(BuildInvocationOutcome::Success);
    }

    pub(super) fn mark_failure(&mut self, error: Option<String>) {
        self.outcome = Some(BuildInvocationOutcome::Failure(error));
    }

    pub(super) fn finish_from_result<T>(&mut self, result: &Result<T>) {
        match result {
            Ok(_) => self.mark_success(),
            Err(err) => self.mark_failure(Some(err.to_string())),
        }
    }
}

impl Drop for BuildStatusGuard {
    fn drop(&mut self) {
        let mut registry = crate::poison::lock(build_status_registry(), "BuildStatusGuard::drop");
        let mut should_remove = false;

        if let Some(entry) = registry.get_mut(&self.project_root) {
            entry.in_flight_count = entry.in_flight_count.saturating_sub(1);

            match self.outcome.take() {
                Some(BuildInvocationOutcome::Success) => {
                    entry.last_failed = false;
                    entry.last_error = None;
                }
                Some(BuildInvocationOutcome::Failure(error)) => {
                    entry.last_failed = true;
                    entry.last_error = error;
                }
                None => {
                    entry.last_failed = true;
                    entry
                        .last_error
                        .get_or_insert_with(|| "build invocation aborted".to_string());
                }
            }

            should_remove =
                entry.in_flight_count == 0 && !entry.last_failed && entry.last_error.is_none();
        }

        if should_remove {
            registry.remove(&self.project_root);
        }
    }
}

#[derive(Debug)]
struct BuildStatusCommandRunner {
    inner: Arc<dyn CommandRunner>,
    failed: AtomicBool,
    last_error: Mutex<Option<String>>,
    guard: BuildStatusGuard,
}

impl CommandRunner for BuildStatusCommandRunner {
    fn run(&self, cwd: &Path, program: &Path, args: &[String]) -> std::io::Result<CommandOutput> {
        let result = self.inner.run(cwd, program, args);

        match &result {
            Ok(output) => {
                if !output.status.success() {
                    self.failed.store(true, Ordering::Relaxed);
                    let mut last_error =
                        crate::poison::lock(&self.last_error, "BuildStatusCommandRunner::run");
                    if last_error.is_none() {
                        *last_error = output
                            .status
                            .code()
                            .filter(|code| *code != 0)
                            .map(|code| format!("command exited with status code {code}"))
                            .or_else(|| {
                                Some(format!(
                                    "command exited with status {status:?}",
                                    status = output.status
                                ))
                            });
                    }
                }
            }
            Err(err) => {
                self.failed.store(true, Ordering::Relaxed);
                let mut last_error =
                    crate::poison::lock(&self.last_error, "BuildStatusCommandRunner::run");
                if last_error.is_none() {
                    *last_error = Some(err.to_string());
                }
            }
        }

        result
    }
}

impl Drop for BuildStatusCommandRunner {
    fn drop(&mut self) {
        if self.failed.load(Ordering::Relaxed) {
            let last_error =
                crate::poison::get_mut(&mut self.last_error, "BuildStatusCommandRunner::drop")
                    .take();
            self.guard.mark_failure(last_error);
        } else {
            self.guard.mark_success();
        }
        // `BuildStatusGuard` drops after this and updates the process-global registry.
    }
}

#[derive(Debug, Clone)]
struct BuildStatusCommandRunnerFactory {
    project_root: PathBuf,
    timeout: Option<Duration>,
}

impl CommandRunnerFactory for BuildStatusCommandRunnerFactory {
    fn build_runner(
        &self,
        cancellation: nova_process::CancellationToken,
    ) -> Arc<dyn CommandRunner> {
        let inner = Arc::new(DefaultCommandRunner {
            timeout: self.timeout,
            cancellation: Some(cancellation),
        });
        Arc::new(BuildStatusCommandRunner {
            inner,
            failed: AtomicBool::new(false),
            last_error: Mutex::new(None),
            guard: BuildStatusGuard::new(&self.project_root),
        })
    }
}

#[derive(Debug)]
struct BuildStatusBazelBuildExecutor {
    workspace_root: PathBuf,
    inner: Arc<dyn BazelBuildExecutor>,
}

impl BazelBuildExecutor for BuildStatusBazelBuildExecutor {
    fn compile(
        &self,
        config: &BazelBspConfig,
        workspace_root: &Path,
        targets: &[String],
        cancellation: nova_process::CancellationToken,
    ) -> anyhow::Result<BspCompileOutcome> {
        let mut status_guard = BuildStatusGuard::new(&self.workspace_root);
        let cancellation_for_inner = cancellation.clone();
        let result = self
            .inner
            .compile(config, workspace_root, targets, cancellation_for_inner);

        match &result {
            Ok(outcome) => {
                if cancellation.is_cancelled() || matches!(outcome.status_code, 2 | 3) {
                    let message = match outcome.status_code {
                        3 => Some("bazel build cancelled".to_string()),
                        2 => Some("bazel build failed".to_string()),
                        _ => Some("bazel build cancelled".to_string()),
                    };
                    status_guard.mark_failure(message);
                } else {
                    status_guard.mark_success();
                }
            }
            Err(err) => status_guard.mark_failure(Some(err.to_string())),
        }

        result
    }
}

pub const BUILD_STATUS_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildStatus {
    Idle,
    Building,
    Failed,
}

fn build_status_string(status: BuildStatus) -> &'static str {
    match status {
        BuildStatus::Idle => "idle",
        BuildStatus::Building => "building",
        BuildStatus::Failed => "failed",
    }
}

pub fn handle_build_status(params: serde_json::Value) -> Result<serde_json::Value> {
    let project_root = super::decode_project_root(params)?;

    if project_root.trim().is_empty() {
        return Err(NovaLspError::InvalidParams(
            "`projectRoot` must not be empty".to_string(),
        ));
    }

    let project_root = PathBuf::from(&project_root);
    let key = canonicalize_project_root(&project_root);
    let orchestrator_snapshot = build_orchestrator_if_present(&key).map(|o| o.status());
    let orchestrator_state = orchestrator_snapshot.as_ref().map(|s| s.state);
    let orchestrator_last_error = orchestrator_snapshot
        .as_ref()
        .and_then(|s| s.last_error.clone());
    let orchestrator_building = matches!(
        orchestrator_state,
        Some(BuildTaskState::Queued | BuildTaskState::Running)
    );
    let orchestrator_failed = matches!(
        orchestrator_state,
        Some(BuildTaskState::Failure | BuildTaskState::Cancelled)
    );

    let bazel_snapshot = bazel_build_orchestrator_if_present(&key)
        .or_else(|| {
            nova_project::bazel_workspace_root(&key)
                .and_then(|workspace_root| bazel_build_orchestrator_if_present(&workspace_root))
        })
        .map(|o| o.status());
    let bazel_state = bazel_snapshot.as_ref().map(|s| s.state);
    let bazel_last_error = bazel_snapshot.as_ref().and_then(|s| s.last_error.clone());
    let bazel_building = matches!(
        bazel_state,
        Some(BuildTaskState::Queued | BuildTaskState::Running)
    );
    let bazel_failed = matches!(
        bazel_state,
        Some(BuildTaskState::Failure | BuildTaskState::Cancelled)
    );

    let (registry_status, registry_last_error) = build_status_snapshot_for_project_root(&key);
    let status =
        if registry_status == BuildStatus::Building || orchestrator_building || bazel_building {
            BuildStatus::Building
        } else if registry_status == BuildStatus::Failed || orchestrator_failed || bazel_failed {
            BuildStatus::Failed
        } else {
            BuildStatus::Idle
        };

    let mut last_error = None;
    if status == BuildStatus::Failed {
        last_error = registry_last_error;
        if last_error.is_none() && orchestrator_failed {
            last_error = orchestrator_last_error;
        }
        if last_error.is_none() && bazel_failed {
            last_error = bazel_last_error;
        }
    }

    Ok(Value::Object({
        let mut value = serde_json::Map::new();
        value.insert(
            "schemaVersion".to_string(),
            Value::from(u64::from(BUILD_STATUS_SCHEMA_VERSION)),
        );
        value.insert(
            "status".to_string(),
            Value::String(build_status_string(status).to_string()),
        );
        value.insert("lastError".to_string(), opt_string_value(last_error));
        value
    }))
}

pub const BUILD_DIAGNOSTICS_SCHEMA_VERSION: u32 = 1;

fn build_diagnostics_result_value(
    target: Option<String>,
    status: BuildTaskState,
    build_id: Option<u64>,
    diagnostics: Vec<nova_core::BuildDiagnostic>,
    source: Option<String>,
    error: Option<String>,
) -> Value {
    Value::Object({
        let mut value = serde_json::Map::new();
        value.insert(
            "schemaVersion".to_string(),
            Value::from(u64::from(BUILD_DIAGNOSTICS_SCHEMA_VERSION)),
        );
        value.insert("target".to_string(), opt_string_value(target));
        value.insert(
            "status".to_string(),
            Value::String(build_task_state_string(status).to_string()),
        );
        value.insert("buildId".to_string(), opt_u64_value(build_id));
        value.insert(
            "diagnostics".to_string(),
            Value::Array(
                diagnostics
                    .into_iter()
                    .map(build_diagnostic_value)
                    .collect(),
            ),
        );
        value.insert("source".to_string(), opt_string_value(source));
        value.insert("error".to_string(), opt_string_value(error));
        value
    })
}

pub fn handle_build_diagnostics(params: serde_json::Value) -> Result<serde_json::Value> {
    let obj = params
        .as_object()
        .ok_or_else(|| NovaLspError::InvalidParams("params must be an object".to_string()))?;
    let project_root = super::decode_project_root(serde_json::Value::Object(obj.clone()))?;
    let target = super::get_str(obj, &["target"]).map(|s| s.to_string());

    if project_root.trim().is_empty() {
        return Err(NovaLspError::InvalidParams(
            "`projectRoot` must not be empty".to_string(),
        ));
    }

    let requested_root = PathBuf::from(&project_root);
    let requested_root =
        canonicalize_best_effort(requested_root.as_path(), "handle_build_diagnostics");

    let snapshot = build_orchestrator_if_present(&requested_root).map(|o| o.diagnostics());
    if let Some(BuildDiagnosticsSnapshot {
        build_id,
        state,
        diagnostics,
        error,
    }) = snapshot
    {
        return Ok(build_diagnostics_result_value(
            target.clone(),
            state,
            build_id,
            diagnostics,
            None,
            error,
        ));
    }

    if let Some(workspace_root) = nova_project::bazel_workspace_root(&requested_root) {
        let snapshot =
            bazel_build_orchestrator_if_present(&workspace_root).map(|o| o.diagnostics());
        let resp = match snapshot {
            Some(BazelBuildDiagnosticsSnapshot {
                build_id,
                state,
                targets,
                diagnostics,
                error,
            }) => build_diagnostics_result_value(
                target.clone().or_else(|| targets.first().cloned()),
                state,
                build_id,
                diagnostics,
                Some("bsp".to_string()),
                error,
            ),
            None => build_diagnostics_result_value(
                target.clone(),
                BuildTaskState::Idle,
                None,
                Vec::new(),
                None,
                None,
            ),
        };

        return Ok(resp);
    }

    Ok(build_diagnostics_result_value(
        target,
        BuildTaskState::Idle,
        None,
        Vec::new(),
        None,
        None,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nova_test_utils::EnvVarGuard;
    use serde_json::Value;
    use std::sync::{Mutex, OnceLock};
    #[cfg(unix)]
    use std::{fs, os::unix::fs::PermissionsExt};
    use tempfile::TempDir;

    fn project_root_params(project_root: impl Into<String>) -> Value {
        Value::Object({
            let mut obj = serde_json::Map::new();
            obj.insert(
                "projectRoot".to_string(),
                serde_json::Value::String(project_root.into()),
            );
            obj
        })
    }

    fn file_classpath_params(project_root: impl Into<String>, uri: impl Into<String>) -> Value {
        Value::Object({
            let mut obj = serde_json::Map::new();
            obj.insert(
                "projectRoot".to_string(),
                serde_json::Value::String(project_root.into()),
            );
            obj.insert("uri".to_string(), serde_json::Value::String(uri.into()));
            obj
        })
    }

    fn gradle_compile_config_payload(compile_classpath: Option<Vec<String>>) -> Value {
        Value::Object({
            let mut obj = serde_json::Map::new();
            obj.insert(
                "compileClasspath".to_string(),
                compile_classpath.map_or(Value::Null, |v| {
                    serde_json::Value::Array(v.into_iter().map(Value::String).collect())
                }),
            );
            obj
        })
    }

    fn gradle_batch_project_payload(
        path: impl Into<String>,
        project_dir: impl Into<String>,
        config: Value,
    ) -> Value {
        Value::Object({
            let mut obj = serde_json::Map::new();
            obj.insert("path".to_string(), serde_json::Value::String(path.into()));
            obj.insert(
                "projectDir".to_string(),
                serde_json::Value::String(project_dir.into()),
            );
            obj.insert("config".to_string(), config);
            obj
        })
    }

    fn gradle_batch_payload(projects: Vec<Value>) -> Value {
        Value::Object({
            let mut obj = serde_json::Map::new();
            obj.insert("projects".to_string(), serde_json::Value::Array(projects));
            obj
        })
    }

    #[test]
    fn params_accepts_project_root_aliases() {
        let mut params_obj = serde_json::Map::new();
        params_obj.insert(
            "root".to_string(),
            serde_json::Value::String("/tmp/project".to_string()),
        );
        params_obj.insert(
            "kind".to_string(),
            serde_json::Value::String("maven".to_string()),
        );
        params_obj.insert(
            "project_path".to_string(),
            serde_json::Value::String(":app".to_string()),
        );
        let params = parse_params(serde_json::Value::Object(params_obj)).expect("params");

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
    fn cached_bazel_workspace_is_reused_and_reset() {
        let _lock = nova_test_utils::env_lock();
        let cache_dir = TempDir::new().unwrap();
        let _cache_guard = EnvVarGuard::set("NOVA_CACHE_DIR", cache_dir.path());

        let workspace_root = TempDir::new().unwrap();
        std::fs::write(workspace_root.path().join("WORKSPACE"), "").unwrap();

        let first = cached_bazel_workspace_for_root(workspace_root.path()).unwrap();
        let second = cached_bazel_workspace_for_root(workspace_root.path()).unwrap();
        assert!(Arc::ptr_eq(&first, &second));

        reset_cached_bazel_workspace(workspace_root.path());
        let third = cached_bazel_workspace_for_root(workspace_root.path()).unwrap();
        assert!(!Arc::ptr_eq(&first, &third));

        reset_cached_bazel_workspace(workspace_root.path());
    }

    #[test]
    fn load_build_metadata_resolves_gradle_project_dir_override() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::write(
            root.join("settings.gradle"),
            "include ':app'\nproject(':app').projectDir = file('modules/application')\n",
        )
        .unwrap();

        let src_root = root.join("modules/application/src/main/java");
        std::fs::create_dir_all(&src_root).unwrap();
        std::fs::write(src_root.join("Hello.java"), "class Hello {}").unwrap();

        let params = ProjectParams {
            project_root: root.to_string_lossy().to_string(),
            build_tool: Some(BuildTool::Gradle),
            module: None,
            project_path: Some(":app".to_string()),
            target: None,
        };

        let metadata = load_build_metadata(&params);
        let expected = src_root.canonicalize().unwrap();
        let actual: Vec<PathBuf> = metadata.source_roots.iter().map(PathBuf::from).collect();

        assert!(
            actual.iter().any(|root| *root == expected),
            "expected {expected:?} in {actual:?}"
        );
    }

    #[test]
    fn load_build_metadata_resolves_include_flat_module_outside_workspace_root() {
        let tmp = TempDir::new().unwrap();
        let outer = tmp.path();
        let workspace_root = outer.join("workspace");
        std::fs::create_dir_all(&workspace_root).unwrap();
        std::fs::write(
            workspace_root.join("settings.gradle"),
            "includeFlat 'app'\n",
        )
        .unwrap();

        let src_root = outer.join("app/src/main/java");
        std::fs::create_dir_all(&src_root).unwrap();
        std::fs::write(src_root.join("Hello.java"), "class Hello {}").unwrap();

        let params = ProjectParams {
            project_root: workspace_root.to_string_lossy().to_string(),
            build_tool: Some(BuildTool::Gradle),
            module: None,
            project_path: Some(":app".to_string()),
            target: None,
        };

        let metadata = load_build_metadata(&params);
        let expected = src_root.canonicalize().unwrap();
        let actual: Vec<PathBuf> = metadata.source_roots.iter().map(PathBuf::from).collect();

        assert!(
            actual.iter().any(|root| *root == expected),
            "expected {expected:?} in {actual:?}"
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
        let actual: std::option::Option<LanguageLevel> =
            language_level_from_java_compile_config(&cfg);
        assert_eq!(
            actual,
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
        let actual: std::option::Option<LanguageLevel> =
            language_level_from_java_compile_config(&cfg);
        assert_eq!(
            actual,
            Some(LanguageLevel {
                major: 8,
                preview: false
            })
        );
    }

    #[test]
    fn classpath_response_is_backwards_compatible() {
        let value = Value::Object({
            let mut resp = serde_json::Map::new();
            resp.insert(
                "classpath".to_string(),
                string_array_value(vec!["/tmp/classes".to_string()]),
            );
            resp.insert("modulePath".to_string(), string_array_value(Vec::new()));
            resp.insert("sourceRoots".to_string(), string_array_value(Vec::new()));
            resp.insert(
                "generatedSourceRoots".to_string(),
                string_array_value(Vec::new()),
            );
            resp.insert(
                "languageLevel".to_string(),
                language_level_value(LanguageLevel::default()),
            );
            resp.insert(
                "outputDirs".to_string(),
                output_dirs_value(OutputDirs::default()),
            );
            resp
        });
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
    fn build_status_defaults_to_idle() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(build_status_for_project_root(tmp.path()), BuildStatus::Idle);
    }

    #[test]
    fn build_status_is_building_while_guard_held() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        assert_eq!(build_status_for_project_root(root), BuildStatus::Idle);

        let mut guard = BuildStatusGuard::new(root);
        assert_eq!(build_status_for_project_root(root), BuildStatus::Building);
        guard.mark_success();
        drop(guard);

        assert_eq!(build_status_for_project_root(root), BuildStatus::Idle);
    }

    #[test]
    fn build_status_failed_then_idle_after_success() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        {
            let mut guard = BuildStatusGuard::new(root);
            guard.mark_failure(Some("boom".to_string()));
        }
        assert_eq!(build_status_for_project_root(root), BuildStatus::Failed);

        {
            let mut guard = BuildStatusGuard::new(root);
            guard.mark_success();
        }
        assert_eq!(build_status_for_project_root(root), BuildStatus::Idle);
    }

    #[test]
    fn build_status_canonicalizes_project_roots() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let root_with_dot = root.join(".");

        let mut guard = BuildStatusGuard::new(&root_with_dot);
        assert_eq!(build_status_for_project_root(root), BuildStatus::Building);
        guard.mark_success();
        drop(guard);

        {
            let mut guard = BuildStatusGuard::new(root);
            guard.mark_failure(Some("fail".to_string()));
        }
        assert_eq!(
            build_status_for_project_root(&root_with_dot),
            BuildStatus::Failed
        );
    }

    #[test]
    fn build_status_reports_failed_when_orchestrator_failed_without_registry_entry() {
        use std::io;
        use std::sync::Arc;
        use std::time::Duration;

        #[derive(Debug)]
        struct FailingRunner;

        impl CommandRunner for FailingRunner {
            fn run(
                &self,
                _cwd: &Path,
                _program: &Path,
                _args: &[String],
            ) -> io::Result<CommandOutput> {
                Err(io::Error::new(io::ErrorKind::Other, "boom"))
            }
        }

        #[derive(Debug)]
        struct FailingRunnerFactory;

        impl CommandRunnerFactory for FailingRunnerFactory {
            fn build_runner(
                &self,
                _cancellation: nova_process::CancellationToken,
            ) -> Arc<dyn CommandRunner> {
                Arc::new(FailingRunner)
            }
        }

        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("pom.xml"), "<project></project>").unwrap();
        let root = tmp.path().canonicalize().unwrap();

        // Install an orchestrator without build-status instrumentation so `handle_build_status`
        // must consult the orchestrator state, not just the status registry.
        let orchestrator = BuildOrchestrator::with_runner_factory(
            root.clone(),
            root.join(".nova").join("build-cache"),
            Arc::new(FailingRunnerFactory),
        );
        {
            let mut map = crate::poison::lock(
                build_orchestrators(),
                "build_status_test/insert_orchestrator",
            );
            map.insert(root.clone(), orchestrator.clone());
        }

        orchestrator.enqueue(BuildRequest::Maven {
            module_relative: None,
            goal: nova_build::MavenBuildGoal::Compile,
        });

        for _ in 0..200 {
            if matches!(
                orchestrator.status().state,
                BuildTaskState::Failure | BuildTaskState::Cancelled
            ) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        assert!(
            matches!(
                orchestrator.status().state,
                BuildTaskState::Failure | BuildTaskState::Cancelled
            ),
            "expected orchestrator to fail"
        );

        let resp =
            handle_build_status(project_root_params(root.to_string_lossy().to_string())).unwrap();

        assert_eq!(resp.get("status").and_then(|v| v.as_str()), Some("failed"));
        let last_error = resp
            .get("lastError")
            .and_then(|v| v.as_str())
            .expect("expected lastError to be a string");
        assert!(
            last_error.contains("boom"),
            "expected lastError to include the runner error: {resp:?}"
        );
    }

    #[test]
    fn target_classpath_requires_target_for_bazel_projects() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("WORKSPACE"), "").unwrap();

        let err = handle_target_classpath(project_root_params(
            tmp.path().to_string_lossy().to_string(),
        ))
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("`target` must be provided for Bazel projects"));
    }

    #[test]
    fn file_classpath_requires_uri() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("WORKSPACE"), "").unwrap();

        let err = handle_file_classpath(project_root_params(
            tmp.path().to_string_lossy().to_string(),
        ))
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("`uri` must be provided"));
    }

    #[test]
    fn file_classpath_requires_file_uri() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("WORKSPACE"), "").unwrap();

        let err = handle_file_classpath(file_classpath_params(
            tmp.path().to_string_lossy().to_string(),
            "http://example.com/Hello.java".to_string(),
        ))
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("`uri` must be a file:// URI"));
    }

    #[test]
    fn file_classpath_requires_valid_uri() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("WORKSPACE"), "").unwrap();

        let err = handle_file_classpath(file_classpath_params(
            tmp.path().to_string_lossy().to_string(),
            "not a uri".to_string(),
        ))
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("invalid uri:"));
    }

    #[test]
    fn project_model_params_accepts_project_root_aliases() {
        let mut params_obj = serde_json::Map::new();
        params_obj.insert(
            "root".to_string(),
            serde_json::Value::String("/tmp/project".to_string()),
        );
        let params = parse_params(serde_json::Value::Object(params_obj)).expect("params");
        assert_eq!(params.project_root, "/tmp/project");
    }

    #[test]
    fn project_model_result_roundtrips_through_json() {
        let value = project_model_result_value(
            "/workspace".into(),
            vec![
                project_model_unit_value(
                    "maven",
                    "module",
                    ".".into(),
                    vec!["/workspace/target/classes".into()],
                    Vec::new(),
                    vec!["/workspace/src/main/java".into()],
                    Some((Some("17".into()), Some("17".into()), None)),
                ),
                project_model_unit_value(
                    "gradle",
                    "projectPath",
                    ":app".into(),
                    vec!["/workspace/app/build/classes/java/main".into()],
                    Vec::new(),
                    vec!["/workspace/app/src/main/java".into()],
                    Some((Some("17".into()), Some("17".into()), Some("17".into()))),
                ),
                project_model_unit_value(
                    "bazel",
                    "target",
                    "//java/com/example:lib".into(),
                    vec!["/workspace/bazel-out/lib.jar".into()],
                    vec!["/workspace/bazel-out/module.jar".into()],
                    vec!["/workspace/java/com/example".into()],
                    Some((Some("17".into()), Some("17".into()), None)),
                ),
            ],
        );

        let encoded = serde_json::to_vec(&value).unwrap();
        let decoded: Value = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded, value);
    }

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn path_env_or_empty() -> String {
        match std::env::var("PATH") {
            Ok(path) => path,
            Err(_) => String::new(),
        }
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
        let _guard =
            crate::poison::lock(env_lock(), "extensions/build/test/target_classpath_maven");
        let original_path = path_env_or_empty();

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
            &format!(
                "#!/bin/sh\n\
\n\
for arg in \"$@\"; do\n\
  case \"$arg\" in\n\
    -Dexpression=project.compileClasspathElements|-Dexpression=project.testClasspathElements)\n\
      echo \"{fake_jar_str}\"\n\
      exit 0\n\
      ;;\n\
    -Dexpression=project.compileSourceRoots|-Dexpression=project.testCompileSourceRoots|-Dexpression=project.testSourceRoots)\n\
      echo \"[]\"\n\
      exit 0\n\
      ;;\n\
    -Dexpression=maven.compiler.source|-Dexpression=maven.compiler.target)\n\
      echo \"17\"\n\
      exit 0\n\
      ;;\n\
    -Dexpression=maven.compiler.release|-Dexpression=project.build.outputDirectory|-Dexpression=project.build.testOutputDirectory)\n\
      echo \"null\"\n\
      exit 0\n\
      ;;\n\
    -Dexpression=maven.compiler.compilerArgs|-Dexpression=maven.compiler.compilerArgument)\n\
      echo \"null\"\n\
      exit 0\n\
      ;;\n\
  esac\n\
done\n\
\n\
echo \"null\"\n",
            ),
        );

        std::env::set_var("PATH", format!("{}:{}", bin_dir.display(), original_path));

        let resp = handle_target_classpath(project_root_params(root.to_string_lossy().to_string()))
            .unwrap();

        std::env::set_var("PATH", original_path);

        let classpath = resp
            .get("classpath")
            .and_then(|v| v.as_array())
            .expect("classpath array");
        assert!(
            classpath
                .iter()
                .filter_map(|v| v.as_str())
                .any(|p| p == fake_jar_str),
            "classpath should include entry from mocked `mvn`"
        );
    }

    #[test]
    #[cfg(unix)]
    fn target_classpath_uses_build_manager_for_gradle() {
        let _guard =
            crate::poison::lock(env_lock(), "extensions/build/test/target_classpath_gradle");
        let original_path = path_env_or_empty();

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
            &format!(
                "#!/bin/sh\n\ncat <<'EOF'\nNOVA_JSON_BEGIN\n{{\"compileClasspath\":[\"{}\"]}}\nNOVA_JSON_END\nEOF\n",
                &fake_jar_str
            ),
        );

        std::env::set_var("PATH", format!("{}:{}", bin_dir.display(), original_path));

        let resp = handle_target_classpath(project_root_params(root.to_string_lossy().to_string()))
            .unwrap();

        std::env::set_var("PATH", original_path);

        let classpath = resp
            .get("classpath")
            .and_then(|v| v.as_array())
            .expect("classpath array");
        assert!(
            classpath
                .iter()
                .filter_map(|v| v.as_str())
                .any(|p| p == fake_jar_str),
            "classpath should include entry from mocked `gradle`"
        );
    }

    #[test]
    #[cfg(unix)]
    fn project_model_uses_batch_gradle_task_for_multi_module_workspaces() {
        let _guard = crate::poison::lock(
            env_lock(),
            "extensions/build/test/project_model_uses_batch_gradle_task",
        );
        let original_path = path_env_or_empty();

        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        // Minimal multi-module Gradle markers (used by `nova-project` for module discovery).
        fs::write(root.join("settings.gradle"), "include ':app', ':lib'\n").unwrap();
        fs::write(root.join("build.gradle"), "").unwrap();
        fs::create_dir_all(root.join("app")).unwrap();
        fs::create_dir_all(root.join("lib")).unwrap();
        fs::write(root.join("app/build.gradle"), "plugins { id 'java' }\n").unwrap();
        fs::write(root.join("lib/build.gradle"), "plugins { id 'java' }\n").unwrap();

        // Fake Gradle executable that emits Nova sentinels + counts invocations.
        let bin_dir = root.join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let counter = root.join("gradle-invocations.txt");

        let shared = root.join("shared.jar");
        let app_dep = root.join("app.jar");
        let lib_dep = root.join("lib.jar");
        fs::write(&shared, "").unwrap();
        fs::write(&app_dep, "").unwrap();
        fs::write(&lib_dep, "").unwrap();

        let batch_payload = gradle_batch_payload(vec![
            gradle_batch_project_payload(
                ":".to_string(),
                root.to_string_lossy().to_string(),
                gradle_compile_config_payload(None),
            ),
            gradle_batch_project_payload(
                ":app".to_string(),
                root.join("app").to_string_lossy().to_string(),
                gradle_compile_config_payload(Some(vec![
                    shared.to_string_lossy().to_string(),
                    app_dep.to_string_lossy().to_string(),
                ])),
            ),
            gradle_batch_project_payload(
                ":lib".to_string(),
                root.join("lib").to_string_lossy().to_string(),
                gradle_compile_config_payload(Some(vec![
                    shared.to_string_lossy().to_string(),
                    lib_dep.to_string_lossy().to_string(),
                ])),
            ),
        ]);

        let root_payload = gradle_compile_config_payload(None);
        let app_payload = gradle_compile_config_payload(Some(vec![
            shared.to_string_lossy().to_string(),
            app_dep.to_string_lossy().to_string(),
        ]));
        let lib_payload = gradle_compile_config_payload(Some(vec![
            shared.to_string_lossy().to_string(),
            lib_dep.to_string_lossy().to_string(),
        ]));

        write_executable(
            &bin_dir.join("gradle"),
            &format!(
                "#!/bin/sh\n\
set -eu\n\
\n\
echo 1 >> \"{}\"\n\
\n\
last=\"\"\n\
for arg in \"$@\"; do\n\
  last=\"$arg\"\n\
done\n\
\n\
case \"$last\" in\n\
  printNovaAllJavaCompileConfigs)\n\
    cat <<'EOF'\n\
NOVA_ALL_JSON_BEGIN\n\
{}\n\
NOVA_ALL_JSON_END\n\
EOF\n\
    ;;\n\
  printNovaJavaCompileConfig)\n\
    cat <<'EOF'\n\
NOVA_JSON_BEGIN\n\
{}\n\
NOVA_JSON_END\n\
EOF\n\
    ;;\n\
  :app:printNovaJavaCompileConfig)\n\
    cat <<'EOF'\n\
NOVA_JSON_BEGIN\n\
{}\n\
NOVA_JSON_END\n\
EOF\n\
    ;;\n\
  :lib:printNovaJavaCompileConfig)\n\
    cat <<'EOF'\n\
NOVA_JSON_BEGIN\n\
{}\n\
NOVA_JSON_END\n\
EOF\n\
    ;;\n\
  *)\n\
    echo \"unexpected gradle task: $last\" >&2\n\
    exit 1\n\
    ;;\n\
esac\n",
                counter.to_string_lossy(),
                batch_payload,
                root_payload,
                app_payload,
                lib_payload,
            ),
        );

        std::env::set_var("PATH", format!("{}:{}", bin_dir.display(), original_path));

        let value =
            handle_project_model(project_root_params(root.to_string_lossy().to_string())).unwrap();

        std::env::set_var("PATH", original_path);

        assert_eq!(
            value.get("projectRoot").and_then(|v| v.as_str()),
            Some(root.to_string_lossy().as_ref())
        );
        assert_eq!(
            value.get("units").and_then(|v| v.as_array()).map(Vec::len),
            Some(2)
        );

        let count = fs::read_to_string(&counter)
            .expect("read gradle invocation counter")
            .lines()
            .count();
        assert_eq!(count, 1, "expected 1 gradle invocation, got {count}");
    }

    #[test]
    #[cfg(unix)]
    fn project_model_uses_batch_gradle_task_with_settings_project_dir_overrides() {
        let _guard = crate::poison::lock(
            env_lock(),
            "extensions/build/test/project_model_batch_gradle_task_project_dir_overrides",
        );
        let original_path = path_env_or_empty();

        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        fs::write(
            root.join("settings.gradle"),
            "include ':app', ':lib'\n\
project(':app').projectDir = file('modules/application')\n\
project(':lib').projectDir = file('modules/library')\n",
        )
        .unwrap();
        fs::write(root.join("build.gradle"), "").unwrap();
        fs::create_dir_all(root.join("modules/application")).unwrap();
        fs::create_dir_all(root.join("modules/library")).unwrap();
        fs::write(
            root.join("modules/application/build.gradle"),
            "plugins { id 'java' }\n",
        )
        .unwrap();
        fs::write(
            root.join("modules/library/build.gradle"),
            "plugins { id 'java' }\n",
        )
        .unwrap();

        // Fake Gradle executable that emits batch configs for `:app` / `:lib` and fails for the
        // filesystem-derived project paths (`:modules:application`, `:modules:library`).
        let bin_dir = root.join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let counter = root.join("gradle-invocations.txt");

        let app_dep = root.join("app.jar");
        let lib_dep = root.join("lib.jar");
        fs::write(&app_dep, "").unwrap();
        fs::write(&lib_dep, "").unwrap();

        let batch_payload = gradle_batch_payload(vec![
            gradle_batch_project_payload(
                ":".to_string(),
                root.to_string_lossy().to_string(),
                gradle_compile_config_payload(None),
            ),
            gradle_batch_project_payload(
                ":app".to_string(),
                root.join("modules/application")
                    .to_string_lossy()
                    .to_string(),
                gradle_compile_config_payload(Some(vec![app_dep.to_string_lossy().to_string()])),
            ),
            gradle_batch_project_payload(
                ":lib".to_string(),
                root.join("modules/library").to_string_lossy().to_string(),
                gradle_compile_config_payload(Some(vec![lib_dep.to_string_lossy().to_string()])),
            ),
        ]);

        write_executable(
            &bin_dir.join("gradle"),
            &format!(
                "#!/bin/sh\n\
set -eu\n\
\n\
echo 1 >> \"{}\"\n\
\n\
last=\"\"\n\
for arg in \"$@\"; do\n\
  last=\"$arg\"\n\
done\n\
\n\
case \"$last\" in\n\
  printNovaAllJavaCompileConfigs)\n\
    cat <<'EOF'\n\
NOVA_ALL_JSON_BEGIN\n\
{}\n\
NOVA_ALL_JSON_END\n\
EOF\n\
    ;;\n\
  :modules:application:printNovaJavaCompileConfig|:modules:library:printNovaJavaCompileConfig)\n\
    echo \"unexpected gradle task (filesystem path used as project path): $last\" >&2\n\
    exit 1\n\
    ;;\n\
  :app:printNovaJavaCompileConfig|:lib:printNovaJavaCompileConfig)\n\
    echo \"unexpected per-project gradle invocation: $last\" >&2\n\
    exit 1\n\
    ;;\n\
  *)\n\
    echo \"unexpected gradle task: $last\" >&2\n\
    exit 1\n\
    ;;\n\
esac\n",
                counter.to_string_lossy(),
                batch_payload,
            ),
        );

        std::env::set_var("PATH", format!("{}:{}", bin_dir.display(), original_path));

        let value =
            handle_project_model(project_root_params(root.to_string_lossy().to_string())).unwrap();

        std::env::set_var("PATH", original_path);

        assert_eq!(
            value.get("projectRoot").and_then(|v| v.as_str()),
            Some(root.to_string_lossy().as_ref())
        );
        let units = value
            .get("units")
            .and_then(|v| v.as_array())
            .expect("units array");
        assert_eq!(units.len(), 2);

        let paths: Vec<_> = units
            .iter()
            .map(|unit| {
                assert_eq!(unit.get("kind").and_then(|v| v.as_str()), Some("gradle"));
                unit.get("projectPath")
                    .and_then(|v| v.as_str())
                    .expect("projectPath")
            })
            .collect();
        assert_eq!(paths, vec![":app", ":lib"]);

        let count = fs::read_to_string(&counter)
            .expect("read gradle invocation counter")
            .lines()
            .count();
        assert_eq!(count, 1, "expected 1 gradle invocation, got {count}");
    }

    #[test]
    #[cfg(unix)]
    fn project_model_resolves_buildsrc_via_gradle_wrapper_project_dir() {
        let _guard = crate::poison::lock(
            env_lock(),
            "extensions/build/test/project_model_resolves_buildsrc_via_gradle_wrapper",
        );
        let original_path = path_env_or_empty();

        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        fs::write(root.join("settings.gradle"), "include ':app'\n").unwrap();
        fs::write(root.join("build.gradle"), "").unwrap();

        fs::create_dir_all(root.join("app")).unwrap();
        fs::write(root.join("app/build.gradle"), "plugins { id 'java' }\n").unwrap();

        // `buildSrc` is a separate Gradle build; `nova-project` models it via a synthetic Gradle
        // project path `:__buildSrc` which should *not* be used as a task prefix when invoking the
        // root build.
        fs::create_dir_all(root.join("buildSrc/src/main/java")).unwrap();
        fs::write(
            root.join("buildSrc/build.gradle"),
            "plugins { id 'java' }\n",
        )
        .unwrap();

        let app_dep = root.join("app.jar");
        let buildsrc_dep = root.join("buildsrc.jar");
        fs::write(&app_dep, "").unwrap();
        fs::write(&buildsrc_dep, "").unwrap();

        let batch_payload = gradle_batch_payload(vec![
            gradle_batch_project_payload(
                ":".to_string(),
                root.to_string_lossy().to_string(),
                gradle_compile_config_payload(None),
            ),
            gradle_batch_project_payload(
                ":app".to_string(),
                root.join("app").to_string_lossy().to_string(),
                gradle_compile_config_payload(Some(vec![app_dep.to_string_lossy().to_string()])),
            ),
        ]);
        let buildsrc_payload =
            gradle_compile_config_payload(Some(vec![buildsrc_dep.to_string_lossy().to_string()]));

        let bin_dir = root.join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let counter = root.join("gradle-invocations.txt");

        // Guardrail: `nova-build` should prefer `./gradlew` when present. If `handle_project_model`
        // ever falls back to invoking `gradle` directly (e.g. by `chdir`ing into `buildSrc/`), we
        // want the test to fail deterministically even if a system Gradle exists on the runner.
        write_executable(
            &bin_dir.join("gradle"),
            "#!/bin/sh\n\
set -eu\n\
echo \"unexpected system gradle invocation\" >&2\n\
exit 1\n",
        );

        write_executable(
            &root.join("gradlew"),
            &format!(
                "#!/bin/sh\n\
set -eu\n\
\n\
echo 1 >> \"{counter}\"\n\
\n\
has_project_dir=0\n\
project_dir=\"\"\n\
prev=\"\"\n\
last=\"\"\n\
for arg in \"$@\"; do\n\
  last=\"$arg\"\n\
  if [ \"$prev\" = \"--project-dir\" ]; then\n\
    has_project_dir=1\n\
    project_dir=\"$arg\"\n\
  fi\n\
  prev=\"$arg\"\n\
done\n\
\n\
if [ \"$has_project_dir\" = 1 ]; then\n\
  if [ \"$project_dir\" != \"buildSrc\" ]; then\n\
    echo \"unexpected --project-dir: $project_dir\" >&2\n\
    exit 1\n\
  fi\n\
  case \"$last\" in\n\
    :__buildSrc:printNovaJavaCompileConfig)\n\
      echo \"unexpected gradle task (synthetic buildSrc project path used as task prefix): $last\" >&2\n\
      exit 1\n\
      ;;\n\
    printNovaJavaCompileConfig)\n\
      cat <<'EOF'\n\
NOVA_JSON_BEGIN\n\
{buildsrc_payload}\n\
NOVA_JSON_END\n\
EOF\n\
      ;;\n\
    *)\n\
      echo \"unexpected gradle task for buildSrc build: $last\" >&2\n\
      exit 1\n\
      ;;\n\
  esac\n\
else\n\
  case \"$last\" in\n\
    printNovaAllJavaCompileConfigs)\n\
      cat <<'EOF'\n\
NOVA_ALL_JSON_BEGIN\n\
{batch_payload}\n\
NOVA_ALL_JSON_END\n\
EOF\n\
      ;;\n\
    :__buildSrc:printNovaJavaCompileConfig)\n\
      echo \"unexpected gradle task (synthetic buildSrc project path used as task prefix): $last\" >&2\n\
      exit 1\n\
      ;;\n\
    *)\n\
      echo \"unexpected gradle task in root build: $last\" >&2\n\
      exit 1\n\
      ;;\n\
  esac\n\
fi\n",
                counter = counter.to_string_lossy(),
                batch_payload = batch_payload,
                buildsrc_payload = buildsrc_payload,
            ),
        );

        std::env::set_var("PATH", format!("{}:{}", bin_dir.display(), original_path));

        let value =
            handle_project_model(project_root_params(root.to_string_lossy().to_string())).unwrap();

        std::env::set_var("PATH", original_path);

        assert_eq!(
            value.get("projectRoot").and_then(|v| v.as_str()),
            Some(root.to_string_lossy().as_ref())
        );
        let units = value
            .get("units")
            .and_then(|v| v.as_array())
            .expect("units array");
        assert_eq!(units.len(), 2);

        let mut units_by_path: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for unit in units {
            assert_eq!(unit.get("kind").and_then(|v| v.as_str()), Some("gradle"));
            let path = unit
                .get("projectPath")
                .and_then(|v| v.as_str())
                .expect("projectPath");
            let compile_classpath = unit
                .get("compileClasspath")
                .and_then(|v| v.as_array())
                .expect("compileClasspath array")
                .iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.to_string())
                .collect::<Vec<_>>();
            units_by_path.insert(path.to_string(), compile_classpath);
        }

        let app_dep_str = app_dep.to_string_lossy().to_string();
        let buildsrc_dep_str = buildsrc_dep.to_string_lossy().to_string();

        assert!(
            units_by_path
                .get(":app")
                .is_some_and(|cp| cp.iter().any(|p| p == &app_dep_str)),
            "expected :app unit to include jar from mocked `gradle`: {units_by_path:?}"
        );
        assert!(
            units_by_path
                .get(":__buildSrc")
                .is_some_and(|cp| cp.iter().any(|p| p == &buildsrc_dep_str)),
            "expected :__buildSrc unit to include jar from mocked `gradle`: {units_by_path:?}"
        );

        let count = fs::read_to_string(&counter)
            .expect("read gradle invocation counter")
            .lines()
            .count();
        assert_eq!(count, 2, "expected 2 gradle invocations, got {count}");
    }

    #[test]
    #[cfg(unix)]
    fn project_model_uses_gradle_project_path_from_settings_project_dir_override() {
        let _guard = crate::poison::lock(
            env_lock(),
            "extensions/build/test/project_model_gradle_project_path_project_dir_override",
        );
        let original_path = path_env_or_empty();

        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        fs::write(
            root.join("settings.gradle"),
            "include ':app'\nproject(':app').projectDir = file('modules/application')\n",
        )
        .unwrap();
        fs::create_dir_all(root.join("modules/application")).unwrap();
        fs::write(
            root.join("modules/application/build.gradle"),
            "plugins { id 'java' }\n",
        )
        .unwrap();

        let bin_dir = root.join("bin");
        fs::create_dir_all(&bin_dir).unwrap();

        let fake_jar = root.join("app.jar");
        fs::write(&fake_jar, "").unwrap();
        let fake_jar_str = fake_jar.to_string_lossy().to_string();

        write_executable(
            &bin_dir.join("gradle"),
            &format!(
                "#!/bin/sh\n\
set -eu\n\
\n\
last=\"\"\n\
for arg in \"$@\"; do\n\
  last=\"$arg\"\n\
done\n\
\n\
case \"$last\" in\n\
  :app:printNovaJavaCompileConfig)\n\
    cat <<'EOF'\n\
NOVA_JSON_BEGIN\n\
{{\"compileClasspath\":[\"{fake_jar_str}\"]}}\n\
NOVA_JSON_END\n\
EOF\n\
    ;;\n\
  :modules:application:printNovaJavaCompileConfig)\n\
    echo \"unexpected gradle task (filesystem path used as project path): $last\" >&2\n\
    exit 1\n\
    ;;\n\
  *)\n\
    echo \"unexpected gradle task: $last\" >&2\n\
    exit 1\n\
    ;;\n\
esac\n",
            ),
        );

        std::env::set_var("PATH", format!("{}:{}", bin_dir.display(), original_path));

        let value =
            handle_project_model(project_root_params(root.to_string_lossy().to_string())).unwrap();

        std::env::set_var("PATH", original_path);

        assert_eq!(
            value.get("projectRoot").and_then(|v| v.as_str()),
            Some(root.to_string_lossy().as_ref())
        );
        let units = value
            .get("units")
            .and_then(|v| v.as_array())
            .expect("units array");
        assert_eq!(units.len(), 1);
        let unit = &units[0];
        assert_eq!(unit.get("kind").and_then(|v| v.as_str()), Some("gradle"));
        assert_eq!(
            unit.get("projectPath").and_then(|v| v.as_str()),
            Some(":app")
        );
        let compile_classpath = unit
            .get("compileClasspath")
            .and_then(|v| v.as_array())
            .expect("compileClasspath array");
        assert!(
            compile_classpath
                .iter()
                .filter_map(|v| v.as_str())
                .any(|p| p == fake_jar_str),
            "expected compile classpath to include jar from mocked `gradle`: {compile_classpath:?}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn project_model_uses_gradle_project_path_for_include_flat_modules() {
        let _guard = crate::poison::lock(
            env_lock(),
            "extensions/build/test/project_model_gradle_project_path_include_flat",
        );
        let original_path = path_env_or_empty();

        // `includeFlat` references a sibling directory of the Gradle workspace root.
        let tmp = TempDir::new().unwrap();
        let workspace_root = tmp.path().join("workspace");
        let included_root = tmp.path().join("application");
        fs::create_dir_all(&workspace_root).unwrap();
        fs::create_dir_all(&included_root).unwrap();

        fs::write(
            workspace_root.join("settings.gradle"),
            "includeFlat 'application'\n",
        )
        .unwrap();
        fs::write(workspace_root.join("build.gradle"), "").unwrap();
        fs::write(
            included_root.join("build.gradle"),
            "plugins { id 'java' }\n",
        )
        .unwrap();

        let bin_dir = workspace_root.join("bin");
        fs::create_dir_all(&bin_dir).unwrap();

        let fake_jar = workspace_root.join("flat.jar");
        fs::write(&fake_jar, "").unwrap();
        let fake_jar_str = fake_jar.to_string_lossy().to_string();

        write_executable(
            &bin_dir.join("gradle"),
            &format!(
                "#!/bin/sh\n\
set -eu\n\
\n\
last=\"\"\n\
for arg in \"$@\"; do\n\
  last=\"$arg\"\n\
done\n\
\n\
case \"$last\" in\n\
  :application:printNovaJavaCompileConfig)\n\
    cat <<'EOF'\n\
NOVA_JSON_BEGIN\n\
{{\"compileClasspath\":[\"{fake_jar_str}\"]}}\n\
NOVA_JSON_END\n\
EOF\n\
    ;;\n\
  *)\n\
    echo \"unexpected gradle task: $last\" >&2\n\
    exit 1\n\
    ;;\n\
esac\n",
            ),
        );

        std::env::set_var("PATH", format!("{}:{}", bin_dir.display(), original_path));

        let value = handle_project_model(project_root_params(
            workspace_root.to_string_lossy().to_string(),
        ))
        .unwrap();

        std::env::set_var("PATH", original_path);

        assert_eq!(
            value.get("projectRoot").and_then(|v| v.as_str()),
            Some(workspace_root.to_string_lossy().as_ref())
        );
        let units = value
            .get("units")
            .and_then(|v| v.as_array())
            .expect("units array");
        assert_eq!(units.len(), 1);

        let unit = &units[0];
        assert_eq!(unit.get("kind").and_then(|v| v.as_str()), Some("gradle"));
        assert_eq!(
            unit.get("projectPath").and_then(|v| v.as_str()),
            Some(":application")
        );
        let compile_classpath = unit
            .get("compileClasspath")
            .and_then(|v| v.as_array())
            .expect("compileClasspath array");
        assert!(
            compile_classpath
                .iter()
                .filter_map(|v| v.as_str())
                .any(|p| p == fake_jar_str),
            "expected compile classpath to include jar from mocked `gradle`: {compile_classpath:?}"
        );
    }

    #[test]
    fn target_classpath_respects_workspace_config() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/Hello.java"), "class Hello {}").unwrap();
        let generated = tmp.path().join("target/generated-sources/annotations");
        std::fs::create_dir_all(&generated).unwrap();
        std::fs::write(
            tmp.path().join("nova.toml"),
            "[generated_sources]\nenabled = false\n",
        )
        .unwrap();

        let response = handle_target_classpath(project_root_params(
            tmp.path().to_string_lossy().to_string(),
        ))
        .unwrap();

        let roots = response
            .get("sourceRoots")
            .and_then(|value| value.as_array())
            .expect("sourceRoots should be present");
        let generated_text = generated.to_string_lossy();
        assert!(
            !roots
                .iter()
                .any(|root| root.as_str() == Some(generated_text.as_ref())),
            "expected generated sources to be excluded when disabled via nova.toml"
        );
    }
}
