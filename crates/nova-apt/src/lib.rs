use nova_build::{
    collect_gradle_build_files, collect_maven_build_files, BuildError, BuildFileFingerprint,
    BuildManager, BuildResult, CommandRunner, GradleBuildTask, MavenBuildGoal,
};
use nova_build_model::{
    GeneratedRootsSnapshotFile, GeneratedRootsSnapshotModule, GeneratedRootsSnapshotRoot,
    GENERATED_ROOTS_SNAPSHOT_SCHEMA_VERSION,
};
use nova_config::NovaConfig;
use nova_core::fs as core_fs;
use nova_project::{
    load_project_with_options, load_workspace_model_with_options, BuildSystem, LoadOptions, Module,
    ProjectConfig, SourceRoot, SourceRootKind, SourceRootOrigin, WorkspaceModuleBuildId,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;

/// Discover generated Java source roots produced by common annotation processor setups.
///
/// This helper exists for components that only know the workspace root on disk
/// (e.g. lightweight navigation/analysis in fixture tests). When a full
/// [`ProjectConfig`] is available, prefer using its generated [`SourceRoot`]s
/// (origin = `Generated`).
pub fn discover_generated_source_roots(project_root: &Path) -> Vec<PathBuf> {
    let Ok(status) = discover_generated_sources_status(project_root) else {
        return Vec::new();
    };

    let mut roots: Vec<PathBuf> = status
        .modules
        .into_iter()
        .flat_map(|module| module.roots.into_iter().map(|root| root.root.path))
        .filter(|path| path.is_dir())
        .collect();
    roots.sort();
    roots.dedup();
    roots
}

/// Best-effort generated sources discovery + freshness calculation for a workspace root.
///
/// This is a convenience wrapper used by framework analyzers that operate on
/// a workspace folder without a full IDE project model.
///
/// The result respects `nova_config.generated_sources` (enabled/additional/override) and
/// reports stale/missing outputs based on simple mtime comparisons between source roots
/// and generated roots.
pub fn discover_generated_sources_status(
    project_root: &Path,
) -> io::Result<GeneratedSourcesStatus> {
    let workspace_root = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());
    let (config, config_path) = nova_config::load_for_workspace(&workspace_root)
        .unwrap_or_else(|_| (NovaConfig::default(), None));

    let options = LoadOptions {
        nova_config: config.clone(),
        nova_config_path: config_path,
        ..Default::default()
    };

    let project = match load_project_with_options(&workspace_root, &options) {
        Ok(project) => project,
        Err(_) => {
            // If project discovery fails, fall back to a minimal "simple" project
            // rooted at `workspace_root` so we can still surface conventional generated paths.
            //
            // Freshness will be best-effort (missing directories show up as Missing).
            ProjectConfig {
                workspace_root: workspace_root.clone(),
                build_system: BuildSystem::Simple,
                java: nova_project::JavaConfig::default(),
                modules: vec![Module {
                    name: workspace_root
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("root")
                        .to_string(),
                    root: workspace_root.clone(),
                    annotation_processing: Default::default(),
                }],
                jpms_modules: Vec::new(),
                jpms_workspace: None,
                source_roots: vec![SourceRoot {
                    kind: SourceRootKind::Main,
                    origin: SourceRootOrigin::Source,
                    path: workspace_root.join("src"),
                }],
                module_path: Vec::new(),
                classpath: Vec::new(),
                output_dirs: Vec::new(),
                dependencies: Vec::new(),
                workspace_model: None,
            }
        }
    };

    AptManager::new(project, config).status()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GeneratedSourcesFreshness {
    Missing,
    Stale,
    Fresh,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeneratedSourceRootStatus {
    pub root: SourceRoot,
    pub freshness: GeneratedSourcesFreshness,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModuleGeneratedSourcesStatus {
    pub module_name: String,
    pub module_root: std::path::PathBuf,
    pub roots: Vec<GeneratedSourceRootStatus>,
}

#[derive(Clone, Debug)]
pub struct GeneratedSourcesStatus {
    pub enabled: bool,
    pub modules: Vec<ModuleGeneratedSourcesStatus>,
}

#[derive(Clone, Debug)]
pub struct GeneratedSourcesStatusWithBuild {
    pub status: GeneratedSourcesStatus,
    pub build_metadata_error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AptProgressEventKind {
    Begin,
    Report,
    End,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AptProgressEvent {
    pub kind: AptProgressEventKind,
    pub message: String,
    pub module_name: Option<String>,
    pub module_root: Option<PathBuf>,
    pub source_kind: Option<SourceRootKind>,
}

impl AptProgressEvent {
    pub fn begin(message: impl Into<String>) -> Self {
        Self {
            kind: AptProgressEventKind::Begin,
            message: message.into(),
            module_name: None,
            module_root: None,
            source_kind: None,
        }
    }

    pub fn report(message: impl Into<String>) -> Self {
        Self {
            kind: AptProgressEventKind::Report,
            message: message.into(),
            module_name: None,
            module_root: None,
            source_kind: None,
        }
    }

    pub fn end() -> Self {
        Self {
            kind: AptProgressEventKind::End,
            message: "done".to_string(),
            module_name: None,
            module_root: None,
            source_kind: None,
        }
    }

    fn for_module(mut self, module: &Module, kind: SourceRootKind) -> Self {
        self.module_name = Some(module.name.clone());
        self.module_root = Some(module.root.clone());
        self.source_kind = Some(kind);
        self
    }
}

pub trait ProgressReporter {
    fn event(&mut self, event: AptProgressEvent) {
        match event.kind {
            AptProgressEventKind::Begin => self.begin(&event.message),
            AptProgressEventKind::Report => self.report(&event.message),
            AptProgressEventKind::End => self.end(),
        }
    }

    fn begin(&mut self, _title: &str) {}
    fn report(&mut self, _message: &str) {}
    fn end(&mut self) {}
}

pub struct NoopProgressReporter;

impl ProgressReporter for NoopProgressReporter {}

pub struct AptManager {
    project: ProjectConfig,
    config: NovaConfig,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AptRunTarget {
    Workspace,
    MavenModule(PathBuf),
    GradleProject(String),
    BazelTarget(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AptRunStatus {
    /// No modules required (re-)processing; outputs appear fresh.
    UpToDate,
    /// Annotation processing was executed (one or more build tool invocations ran).
    Ran,
    /// The operation was cancelled (best-effort).
    Cancelled,
    /// The operation failed (build tool error, IO error, etc).
    Failed,
}

#[derive(Clone, Debug)]
pub struct AptRunResult {
    pub status: AptRunStatus,
    /// Diagnostics emitted by the build tool (javac output parsing).
    pub diagnostics: Vec<nova_core::Diagnostic>,
    /// Freshness status + generated roots after the run.
    pub generated_sources: GeneratedSourcesStatus,
    /// Whether the result was served from Nova's APT run cache.
    pub cache_hit: bool,
    /// Best-effort error message when `status == Failed`.
    pub error: Option<String>,
}

pub trait AptBuildExecutor {
    fn build_maven(
        &self,
        project_root: &Path,
        module_relative: Option<&Path>,
        goal: MavenBuildGoal,
    ) -> nova_build::Result<BuildResult>;

    fn build_gradle(
        &self,
        project_root: &Path,
        project_path: Option<&str>,
        task: GradleBuildTask,
    ) -> nova_build::Result<BuildResult>;

    fn build_bazel(&self, project_root: &Path, target: &str) -> nova_build::Result<BuildResult>;
}

impl AptBuildExecutor for BuildManager {
    fn build_maven(
        &self,
        project_root: &Path,
        module_relative: Option<&Path>,
        goal: MavenBuildGoal,
    ) -> nova_build::Result<BuildResult> {
        self.build_maven_goal(project_root, module_relative, goal)
    }

    fn build_gradle(
        &self,
        project_root: &Path,
        project_path: Option<&str>,
        task: GradleBuildTask,
    ) -> nova_build::Result<BuildResult> {
        self.build_gradle_task(project_root, project_path, task)
    }

    fn build_bazel(&self, project_root: &Path, target: &str) -> nova_build::Result<BuildResult> {
        let runner = nova_build::DefaultCommandRunner {
            timeout: Some(Duration::from_secs(300)),
            ..Default::default()
        };
        let args = vec!["build".to_string(), target.to_string()];
        let output = runner.run(project_root, Path::new("bazel"), &args)?;
        if output.status.success() {
            return Ok(BuildResult {
                diagnostics: Vec::new(),
                tool: Some("bazel".to_string()),
                command: Some(format!("bazel build {target}")),
                exit_code: output.status.code(),
                stdout: output.stdout,
                stderr: output.stderr,
                output_truncated: output.truncated,
            });
        }

        Err(BuildError::CommandFailed {
            tool: "bazel",
            command: format!("bazel build {target}"),
            code: output.status.code(),
            stdout: output.stdout,
            stderr: output.stderr,
            output_truncated: output.truncated,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MtimeCacheEntry {
    root: PathBuf,
    max_mtime_nanos: Option<u64>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct MtimeCacheFile {
    entries: Vec<MtimeCacheEntry>,
}

static CACHE_TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Persistent cache for `max_java_mtime` computations.
///
/// This cache is best-effort: callers must invalidate paths when files change
/// (e.g. based on workspace file watcher events). Without invalidation, cached
/// mtimes may be stale and can cause incorrect freshness results.
#[derive(Debug)]
pub struct AptMtimeCache {
    cache_path: PathBuf,
    entries: HashMap<PathBuf, Option<u64>>,
    dirty: bool,
}

impl AptMtimeCache {
    pub fn load(workspace_root: &Path) -> io::Result<Self> {
        let cache_path = workspace_root
            .join(".nova")
            .join("apt-cache")
            .join("mtimes.json");
        let file = match std::fs::read_to_string(&cache_path) {
            Ok(text) => serde_json::from_str::<MtimeCacheFile>(&text)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?,
            Err(err) if err.kind() == io::ErrorKind::NotFound => MtimeCacheFile::default(),
            Err(err) => return Err(err),
        };

        let mut entries = HashMap::new();
        for entry in file.entries {
            entries.insert(entry.root, entry.max_mtime_nanos);
        }

        Ok(Self {
            cache_path,
            entries,
            dirty: false,
        })
    }

    pub fn save(&mut self) -> io::Result<()> {
        if !self.dirty {
            return Ok(());
        }

        let Some(parent) = self.cache_path.parent() else {
            return Ok(());
        };
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        std::fs::create_dir_all(parent)?;

        let mut entries: Vec<_> = self
            .entries
            .iter()
            .map(|(root, max_mtime_nanos)| MtimeCacheEntry {
                root: root.clone(),
                max_mtime_nanos: *max_mtime_nanos,
            })
            .collect();
        entries.sort_by(|a, b| a.root.cmp(&b.root));

        let file = MtimeCacheFile { entries };
        let json = serde_json::to_string_pretty(&file)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;

        let (tmp_path, mut file) = open_unique_tmp_file(&self.cache_path, parent)?;
        let write_result = (|| -> io::Result<()> {
            file.write_all(json.as_bytes())?;
            file.sync_all()?;
            Ok(())
        })();
        if let Err(err) = write_result {
            drop(file);
            let _ = std::fs::remove_file(&tmp_path);
            return Err(err);
        }
        drop(file);

        if let Err(err) = rename_overwrite(&tmp_path, &self.cache_path) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(err);
        }

        #[cfg(unix)]
        {
            let _ = std::fs::File::open(parent).and_then(|dir| dir.sync_all());
        }

        self.dirty = false;
        Ok(())
    }

    pub fn invalidate_path(&mut self, path: &Path) {
        let before = self.entries.len();
        self.entries
            .retain(|root, _| !(path.starts_with(root) || root.starts_with(path)));
        if self.entries.len() != before {
            self.dirty = true;
        }
    }

    fn get(&self, root: &Path) -> Option<Option<SystemTime>> {
        self.entries.get(root).copied().map(epoch_nanos_to_time)
    }

    fn insert(&mut self, root: PathBuf, time: Option<SystemTime>) {
        self.entries.insert(root, time_to_epoch_nanos(time));
        self.dirty = true;
    }
}

const APT_RUN_CACHE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum CachedDiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

impl From<nova_core::DiagnosticSeverity> for CachedDiagnosticSeverity {
    fn from(value: nova_core::DiagnosticSeverity) -> Self {
        match value {
            nova_core::DiagnosticSeverity::Error => Self::Error,
            nova_core::DiagnosticSeverity::Warning => Self::Warning,
            nova_core::DiagnosticSeverity::Information => Self::Information,
            nova_core::DiagnosticSeverity::Hint => Self::Hint,
        }
    }
}

impl From<CachedDiagnosticSeverity> for nova_core::DiagnosticSeverity {
    fn from(value: CachedDiagnosticSeverity) -> Self {
        match value {
            CachedDiagnosticSeverity::Error => nova_core::DiagnosticSeverity::Error,
            CachedDiagnosticSeverity::Warning => nova_core::DiagnosticSeverity::Warning,
            CachedDiagnosticSeverity::Information => nova_core::DiagnosticSeverity::Information,
            CachedDiagnosticSeverity::Hint => nova_core::DiagnosticSeverity::Hint,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct CachedPosition {
    line: u32,
    character: u32,
}

impl From<nova_core::Position> for CachedPosition {
    fn from(value: nova_core::Position) -> Self {
        Self {
            line: value.line,
            character: value.character,
        }
    }
}

impl From<CachedPosition> for nova_core::Position {
    fn from(value: CachedPosition) -> Self {
        nova_core::Position::new(value.line, value.character)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct CachedRange {
    start: CachedPosition,
    end: CachedPosition,
}

impl From<nova_core::Range> for CachedRange {
    fn from(value: nova_core::Range) -> Self {
        Self {
            start: value.start.into(),
            end: value.end.into(),
        }
    }
}

impl From<CachedRange> for nova_core::Range {
    fn from(value: CachedRange) -> Self {
        nova_core::Range::new(value.start.into(), value.end.into())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CachedDiagnostic {
    file: PathBuf,
    range: CachedRange,
    severity: CachedDiagnosticSeverity,
    message: String,
    source: Option<String>,
}

impl From<nova_core::Diagnostic> for CachedDiagnostic {
    fn from(value: nova_core::Diagnostic) -> Self {
        Self {
            file: value.file,
            range: value.range.into(),
            severity: value.severity.into(),
            message: value.message,
            source: value.source,
        }
    }
}

impl From<CachedDiagnostic> for nova_core::Diagnostic {
    fn from(value: CachedDiagnostic) -> Self {
        nova_core::Diagnostic {
            file: value.file,
            range: value.range.into(),
            severity: value.severity.into(),
            message: value.message,
            source: value.source,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum CachedSourceRootKind {
    Main,
    Test,
}

impl From<SourceRootKind> for CachedSourceRootKind {
    fn from(value: SourceRootKind) -> Self {
        match value {
            SourceRootKind::Main => Self::Main,
            SourceRootKind::Test => Self::Test,
        }
    }
}

impl From<CachedSourceRootKind> for SourceRootKind {
    fn from(value: CachedSourceRootKind) -> Self {
        match value {
            CachedSourceRootKind::Main => SourceRootKind::Main,
            CachedSourceRootKind::Test => SourceRootKind::Test,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum CachedFreshness {
    Missing,
    Stale,
    Fresh,
}

impl From<GeneratedSourcesFreshness> for CachedFreshness {
    fn from(value: GeneratedSourcesFreshness) -> Self {
        match value {
            GeneratedSourcesFreshness::Missing => Self::Missing,
            GeneratedSourcesFreshness::Stale => Self::Stale,
            GeneratedSourcesFreshness::Fresh => Self::Fresh,
        }
    }
}

impl From<CachedFreshness> for GeneratedSourcesFreshness {
    fn from(value: CachedFreshness) -> Self {
        match value {
            CachedFreshness::Missing => GeneratedSourcesFreshness::Missing,
            CachedFreshness::Stale => GeneratedSourcesFreshness::Stale,
            CachedFreshness::Fresh => GeneratedSourcesFreshness::Fresh,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CachedGeneratedRoot {
    kind: CachedSourceRootKind,
    path: PathBuf,
    freshness: CachedFreshness,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CachedGeneratedModule {
    module_name: String,
    module_root: PathBuf,
    roots: Vec<CachedGeneratedRoot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CachedGeneratedSourcesStatus {
    enabled: bool,
    modules: Vec<CachedGeneratedModule>,
}

impl From<GeneratedSourcesStatus> for CachedGeneratedSourcesStatus {
    fn from(value: GeneratedSourcesStatus) -> Self {
        Self {
            enabled: value.enabled,
            modules: value
                .modules
                .into_iter()
                .map(|module| CachedGeneratedModule {
                    module_name: module.module_name,
                    module_root: module.module_root,
                    roots: module
                        .roots
                        .into_iter()
                        .map(|root| CachedGeneratedRoot {
                            kind: root.root.kind.into(),
                            path: root.root.path,
                            freshness: root.freshness.into(),
                        })
                        .collect(),
                })
                .collect(),
        }
    }
}

impl From<CachedGeneratedSourcesStatus> for GeneratedSourcesStatus {
    fn from(value: CachedGeneratedSourcesStatus) -> Self {
        Self {
            enabled: value.enabled,
            modules: value
                .modules
                .into_iter()
                .map(|module| ModuleGeneratedSourcesStatus {
                    module_name: module.module_name,
                    module_root: module.module_root.clone(),
                    roots: module
                        .roots
                        .into_iter()
                        .map(|root| GeneratedSourceRootStatus {
                            root: SourceRoot {
                                kind: root.kind.into(),
                                origin: SourceRootOrigin::Generated,
                                path: root.path,
                            },
                            freshness: root.freshness.into(),
                        })
                        .collect(),
                })
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedAptRunResult {
    status: AptRunStatus,
    diagnostics: Vec<CachedDiagnostic>,
    generated_sources: CachedGeneratedSourcesStatus,
    error: Option<String>,
}

impl From<AptRunResult> for CachedAptRunResult {
    fn from(value: AptRunResult) -> Self {
        Self {
            status: value.status,
            diagnostics: value
                .diagnostics
                .into_iter()
                .map(CachedDiagnostic::from)
                .collect(),
            generated_sources: value.generated_sources.into(),
            error: value.error,
        }
    }
}

impl From<CachedAptRunResult> for AptRunResult {
    fn from(value: CachedAptRunResult) -> Self {
        Self {
            status: value.status,
            diagnostics: value
                .diagnostics
                .into_iter()
                .map(nova_core::Diagnostic::from)
                .collect(),
            generated_sources: value.generated_sources.into(),
            cache_hit: true,
            error: value.error,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AptRunCacheEntry {
    key: String,
    updated_at_nanos: u64,
    result: CachedAptRunResult,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct AptRunCacheFile {
    schema_version: u32,
    entries: Vec<AptRunCacheEntry>,
}

/// Persistent cache for APT runs.
///
/// This cache is keyed by a stable fingerprint of the build inputs (build files,
/// Nova generated-sources config, and source mtimes). It is best-effort: failures
/// to load or save the cache should not block annotation processing.
#[derive(Debug)]
struct AptRunCache {
    cache_path: PathBuf,
    entries: Vec<AptRunCacheEntry>,
    dirty: bool,
}

impl AptRunCache {
    fn load(workspace_root: &Path) -> io::Result<Self> {
        let cache_path = workspace_root
            .join(".nova")
            .join("apt-cache")
            .join("runs.json");
        let file = match std::fs::read_to_string(&cache_path) {
            Ok(text) => serde_json::from_str::<AptRunCacheFile>(&text)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?,
            Err(err) if err.kind() == io::ErrorKind::NotFound => AptRunCacheFile {
                schema_version: APT_RUN_CACHE_SCHEMA_VERSION,
                entries: Vec::new(),
            },
            Err(err) => return Err(err),
        };

        let entries = if file.schema_version == APT_RUN_CACHE_SCHEMA_VERSION {
            file.entries
        } else {
            Vec::new()
        };

        Ok(Self {
            cache_path,
            entries,
            dirty: false,
        })
    }

    fn get(&mut self, key: &str) -> Option<CachedAptRunResult> {
        let now = SystemTime::now();
        let now_nanos = time_to_epoch_nanos(Some(now)).unwrap_or(0);
        let mut found = None;

        for entry in &mut self.entries {
            if entry.key == key {
                entry.updated_at_nanos = now_nanos;
                found = Some(entry.result.clone());
                self.dirty = true;
                break;
            }
        }

        found
    }

    fn insert(&mut self, key: String, result: CachedAptRunResult) {
        let now = SystemTime::now();
        let now_nanos = time_to_epoch_nanos(Some(now)).unwrap_or(0);

        if let Some(existing) = self.entries.iter_mut().find(|e| e.key == key) {
            existing.updated_at_nanos = now_nanos;
            existing.result = result;
        } else {
            self.entries.push(AptRunCacheEntry {
                key,
                updated_at_nanos: now_nanos,
                result,
            });
        }
        self.dirty = true;
        self.prune();
    }

    fn prune(&mut self) {
        const MAX_ENTRIES: usize = 32;
        if self.entries.len() <= MAX_ENTRIES {
            return;
        }
        self.entries
            .sort_by(|a, b| b.updated_at_nanos.cmp(&a.updated_at_nanos));
        self.entries.truncate(MAX_ENTRIES);
        // Keep output stable for deterministic diffs.
        self.entries.sort_by(|a, b| a.key.cmp(&b.key));
    }

    fn save(&mut self) -> io::Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let Some(parent) = self.cache_path.parent() else {
            return Ok(());
        };
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        std::fs::create_dir_all(parent)?;

        let file = AptRunCacheFile {
            schema_version: APT_RUN_CACHE_SCHEMA_VERSION,
            entries: self.entries.clone(),
        };
        let json = serde_json::to_string_pretty(&file)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;

        let (tmp_path, mut file) = open_unique_tmp_file(&self.cache_path, parent)?;
        let write_result = (|| -> io::Result<()> {
            file.write_all(json.as_bytes())?;
            file.sync_all()?;
            Ok(())
        })();
        if let Err(err) = write_result {
            drop(file);
            let _ = std::fs::remove_file(&tmp_path);
            return Err(err);
        }
        drop(file);

        if let Err(err) = rename_overwrite(&tmp_path, &self.cache_path) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(err);
        }

        #[cfg(unix)]
        {
            let _ = std::fs::File::open(parent).and_then(|dir| dir.sync_all());
        }

        self.dirty = false;
        Ok(())
    }
}

fn open_unique_tmp_file(dest: &Path, parent: &Path) -> io::Result<(PathBuf, std::fs::File)> {
    let file_name = dest
        .file_name()
        .ok_or_else(|| io::Error::other("destination path has no file name"))?;
    let pid = std::process::id();

    loop {
        let counter = CACHE_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut tmp_name = file_name.to_os_string();
        tmp_name.push(format!(".tmp.{pid}.{counter}"));
        let tmp_path = parent.join(tmp_name);

        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
        {
            Ok(file) => return Ok((tmp_path, file)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
}

fn rename_overwrite(src: &Path, dest: &Path) -> io::Result<()> {
    const MAX_RENAME_ATTEMPTS: usize = 1024;
    let mut attempts = 0usize;

    loop {
        match std::fs::rename(src, dest) {
            Ok(()) => return Ok(()),
            Err(err)
                if cfg!(windows)
                    && (err.kind() == io::ErrorKind::AlreadyExists || dest.exists()) =>
            {
                match std::fs::remove_file(dest) {
                    Ok(()) => {}
                    Err(remove_err) if remove_err.kind() == io::ErrorKind::NotFound => {}
                    Err(remove_err) => return Err(remove_err),
                }

                attempts += 1;
                if attempts >= MAX_RENAME_ATTEMPTS {
                    return Err(err);
                }
                continue;
            }
            Err(err) => return Err(err),
        }
    }
}

fn time_to_epoch_nanos(time: Option<SystemTime>) -> Option<u64> {
    let time = time?;
    let duration = time.duration_since(UNIX_EPOCH).ok()?;
    Some(duration.as_nanos().min(u64::MAX as u128) as u64)
}

fn epoch_nanos_to_time(nanos: Option<u64>) -> Option<SystemTime> {
    let nanos = nanos?;
    Some(UNIX_EPOCH + Duration::from_nanos(nanos))
}

trait MtimeProvider {
    fn max_java_mtime(&mut self, root: &Path) -> io::Result<Option<SystemTime>>;
}

struct FsMtimeProvider;

impl MtimeProvider for FsMtimeProvider {
    fn max_java_mtime(&mut self, root: &Path) -> io::Result<Option<SystemTime>> {
        max_java_mtime(root)
    }
}

struct CachedMtimeProvider<'a> {
    cache: &'a mut AptMtimeCache,
}

impl MtimeProvider for CachedMtimeProvider<'_> {
    fn max_java_mtime(&mut self, root: &Path) -> io::Result<Option<SystemTime>> {
        if let Some(cached) = self.cache.get(root) {
            return Ok(cached);
        }
        let value = max_java_mtime(root)?;
        self.cache.insert(root.to_path_buf(), value);
        Ok(value)
    }
}

struct FreshnessCalculator<'a> {
    project: &'a ProjectConfig,
    mtimes: &'a mut dyn MtimeProvider,
    input_cache: HashMap<(PathBuf, SourceRootKind), Option<SystemTime>>,
    output_cache: HashMap<PathBuf, Option<SystemTime>>,
}

impl<'a> FreshnessCalculator<'a> {
    fn new(project: &'a ProjectConfig, mtimes: &'a mut dyn MtimeProvider) -> Self {
        Self {
            project,
            mtimes,
            input_cache: HashMap::new(),
            output_cache: HashMap::new(),
        }
    }

    fn max_input_mtime(
        &mut self,
        module_root: &Path,
        kind: SourceRootKind,
    ) -> io::Result<Option<SystemTime>> {
        let key = (module_root.to_path_buf(), kind);
        if let Some(value) = self.input_cache.get(&key).copied() {
            return Ok(value);
        }

        let mut max_time = None;
        for root in self
            .project
            .source_roots
            .iter()
            .filter(|root| root.origin == SourceRootOrigin::Source)
            .filter(|root| root.kind == kind)
            .filter(|root| root.path.starts_with(module_root))
        {
            let root_time = self.mtimes.max_java_mtime(&root.path)?;
            if let Some(candidate) = root_time {
                max_time = Some(match max_time {
                    Some(existing) if existing >= candidate => existing,
                    _ => candidate,
                });
            }
        }

        self.input_cache.insert(key, max_time);
        Ok(max_time)
    }

    fn max_output_mtime(&mut self, root: &Path) -> io::Result<Option<SystemTime>> {
        if let Some(value) = self.output_cache.get(root).copied() {
            return Ok(value);
        }
        let value = self.mtimes.max_java_mtime(root)?;
        self.output_cache.insert(root.to_path_buf(), value);
        Ok(value)
    }

    fn freshness_for_root(
        &mut self,
        module_root: &Path,
        generated_root: &SourceRoot,
    ) -> io::Result<GeneratedSourcesFreshness> {
        if generated_root.origin != SourceRootOrigin::Generated {
            return Ok(GeneratedSourcesFreshness::Fresh);
        }

        let input_mtime = self.max_input_mtime(module_root, generated_root.kind)?;
        let Some(input_mtime) = input_mtime else {
            // No inputs means nothing can be stale (and missing outputs are not actionable).
            return Ok(GeneratedSourcesFreshness::Fresh);
        };

        if !generated_root.path.is_dir() {
            return Ok(GeneratedSourcesFreshness::Missing);
        }

        let output_mtime = self.max_output_mtime(&generated_root.path)?;
        let Some(output_mtime) = output_mtime else {
            return Ok(GeneratedSourcesFreshness::Missing);
        };

        if input_mtime > output_mtime {
            Ok(GeneratedSourcesFreshness::Stale)
        } else {
            Ok(GeneratedSourcesFreshness::Fresh)
        }
    }
}

impl AptManager {
    pub fn new(project: ProjectConfig, config: NovaConfig) -> Self {
        Self { project, config }
    }

    pub fn project(&self) -> &ProjectConfig {
        &self.project
    }

    pub fn config(&self) -> &NovaConfig {
        &self.config
    }

    pub fn status(&self) -> io::Result<GeneratedSourcesStatus> {
        let enabled = self.config.generated_sources.enabled;
        let mut modules = Vec::new();

        let mut mtime_provider = FsMtimeProvider;
        let mut freshness = FreshnessCalculator::new(&self.project, &mut mtime_provider);

        for module in &self.project.modules {
            let roots = self
                .generated_roots_for_module(module)
                .into_iter()
                .map(|root| {
                    let freshness = freshness.freshness_for_root(&module.root, &root)?;
                    Ok(GeneratedSourceRootStatus { root, freshness })
                })
                .collect::<io::Result<Vec<_>>>()?;

            modules.push(ModuleGeneratedSourcesStatus {
                module_name: module.name.to_string(),
                module_root: module.root.clone(),
                roots,
            });
        }

        Ok(GeneratedSourcesStatus { enabled, modules })
    }

    pub fn status_cached(&self, cache: &mut AptMtimeCache) -> io::Result<GeneratedSourcesStatus> {
        let enabled = self.config.generated_sources.enabled;
        let mut modules = Vec::new();

        let mut mtime_provider = CachedMtimeProvider { cache };
        let mut freshness = FreshnessCalculator::new(&self.project, &mut mtime_provider);

        for module in &self.project.modules {
            let roots = self
                .generated_roots_for_module(module)
                .into_iter()
                .map(|root| {
                    let freshness = freshness.freshness_for_root(&module.root, &root)?;
                    Ok(GeneratedSourceRootStatus { root, freshness })
                })
                .collect::<io::Result<Vec<_>>>()?;

            modules.push(ModuleGeneratedSourcesStatus {
                module_name: module.name.to_string(),
                module_root: module.root.clone(),
                roots,
            });
        }

        Ok(GeneratedSourcesStatus { enabled, modules })
    }

    /// Like [`AptManager::status`] but first attempts to populate per-module annotation processing
    /// configuration from the build tool.
    ///
    /// This is best-effort: if build metadata extraction fails, Nova falls back to conventional
    /// generated source roots.
    pub fn status_with_build(
        &mut self,
        build: &BuildManager,
    ) -> io::Result<GeneratedSourcesStatusWithBuild> {
        let gradle_projects = self.gradle_project_paths();
        let build_metadata_error = self
            .apply_build_annotation_processing(build, gradle_projects.as_ref())
            .err()
            .map(|err| err.to_string());
        let _ = self.write_generated_roots_snapshot();
        let status = self.status()?;
        Ok(GeneratedSourcesStatusWithBuild {
            status,
            build_metadata_error,
        })
    }

    fn write_generated_roots_snapshot(&self) -> io::Result<()> {
        let snapshot_path = self
            .project
            .workspace_root
            .join(".nova")
            .join("apt-cache")
            .join("generated-roots.json");
        let Some(parent) = snapshot_path.parent() else {
            return Ok(());
        };
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        std::fs::create_dir_all(parent)?;

        let mut modules = Vec::new();
        for module in &self.project.modules {
            let mut roots: Vec<GeneratedRootsSnapshotRoot> = self
                .generated_roots_for_module(module)
                .into_iter()
                .map(|root| GeneratedRootsSnapshotRoot {
                    kind: root.kind.into(),
                    path: root.path,
                })
                .collect();
            roots.sort_by(|a, b| a.kind.cmp(&b.kind).then(a.path.cmp(&b.path)));

            modules.push(GeneratedRootsSnapshotModule {
                module_root: module.root.clone(),
                roots,
            });
        }
        modules.sort_by(|a, b| a.module_root.cmp(&b.module_root));

        let file = GeneratedRootsSnapshotFile {
            schema_version: GENERATED_ROOTS_SNAPSHOT_SCHEMA_VERSION,
            modules,
        };
        let json = serde_json::to_string_pretty(&file)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;

        let (tmp_path, mut file) = open_unique_tmp_file(&snapshot_path, parent)?;
        let write_result = (|| -> io::Result<()> {
            file.write_all(json.as_bytes())?;
            file.sync_all()?;
            Ok(())
        })();
        if let Err(err) = write_result {
            drop(file);
            let _ = std::fs::remove_file(&tmp_path);
            return Err(err);
        }
        drop(file);

        if let Err(err) = rename_overwrite(&tmp_path, &snapshot_path) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(err);
        }

        #[cfg(unix)]
        {
            let _ = std::fs::File::open(parent).and_then(|dir| dir.sync_all());
        }

        Ok(())
    }

    fn run_cache_key(
        &self,
        target: &AptRunTarget,
        modules: &[&Module],
        freshness: &mut FreshnessCalculator<'_>,
    ) -> String {
        let mut hasher = Sha256::new();

        hasher.update(b"nova-apt-run-cache");
        hasher.update([0]);
        hasher.update(APT_RUN_CACHE_SCHEMA_VERSION.to_le_bytes());
        hasher.update([0]);
        hasher.update(nova_core::NOVA_VERSION.as_bytes());
        hasher.update([0]);

        let build_system = match self.project.build_system {
            BuildSystem::Maven => "maven",
            BuildSystem::Gradle => "gradle",
            BuildSystem::Bazel => "bazel",
            BuildSystem::Simple => "simple",
        };
        hasher.update(build_system.as_bytes());
        hasher.update([0]);

        if let Ok(target_json) = serde_json::to_vec(target) {
            hasher.update(&target_json);
        } else {
            hasher.update(b"<target-serde-error>");
        }
        hasher.update([0]);

        if let Ok(config_json) = serde_json::to_vec(&self.config.generated_sources) {
            hasher.update(&config_json);
        } else {
            hasher.update(b"<config-serde-error>");
        }
        hasher.update([0]);

        let build_files_fingerprint = match self.project.build_system {
            BuildSystem::Maven => collect_maven_build_files(&self.project.workspace_root)
                .and_then(|files| {
                    Ok(BuildFileFingerprint::from_files(
                        &self.project.workspace_root,
                        files,
                    )?)
                })
                .map(|fp| fp.digest)
                .unwrap_or_else(|_| "<maven-fingerprint-error>".to_string()),
            BuildSystem::Gradle => collect_gradle_build_files(&self.project.workspace_root)
                .and_then(|files| {
                    Ok(BuildFileFingerprint::from_files(
                        &self.project.workspace_root,
                        files,
                    )?)
                })
                .map(|fp| fp.digest)
                .unwrap_or_else(|_| "<gradle-fingerprint-error>".to_string()),
            _ => "<no-build-fingerprint>".to_string(),
        };
        hasher.update(build_files_fingerprint.as_bytes());
        hasher.update([0]);

        let mut module_roots: Vec<_> = modules.iter().map(|m| m.root.clone()).collect();
        module_roots.sort();
        module_roots.dedup();

        for module_root in module_roots {
            let rel = module_root
                .strip_prefix(&self.project.workspace_root)
                .ok()
                .filter(|p| !p.as_os_str().is_empty());
            match rel {
                Some(rel) => hasher.update(rel.to_string_lossy().as_bytes()),
                None => hasher.update(module_root.to_string_lossy().as_bytes()),
            }
            hasher.update([0]);

            for kind in [SourceRootKind::Main, SourceRootKind::Test] {
                let marker: u8;
                let mtime_nanos: u64;
                match freshness.max_input_mtime(&module_root, kind) {
                    Ok(Some(time)) => {
                        marker = 0;
                        mtime_nanos = time_to_epoch_nanos(Some(time)).unwrap_or(0);
                    }
                    Ok(None) => {
                        marker = 1;
                        mtime_nanos = 0;
                    }
                    Err(_) => {
                        marker = 2;
                        mtime_nanos = 0;
                    }
                }
                hasher.update([marker]);
                hasher.update(mtime_nanos.to_le_bytes());
            }
        }

        hex::encode(hasher.finalize())
    }

    /// Run annotation processing for the given target and return a structured result.
    ///
    /// Unlike [`AptManager::run_annotation_processing_for_target`], this method:
    /// - returns a structured outcome instead of erroring on build failures
    /// - uses a persistent mtime cache to avoid rescanning unchanged source roots
    /// - stores successful results in a persistent run cache keyed by a fingerprint of inputs
    /// - is cancellation-aware (best-effort) via [`CancellationToken`]
    pub fn run(
        &mut self,
        build: &BuildManager,
        target: AptRunTarget,
        cancel: Option<CancellationToken>,
        progress: &mut dyn ProgressReporter,
    ) -> io::Result<AptRunResult> {
        progress.event(AptProgressEvent::begin("Running annotation processing"));

        if cancel.as_ref().is_some_and(|token| token.is_cancelled()) {
            progress.event(AptProgressEvent::end());
            return Ok(AptRunResult {
                status: AptRunStatus::Cancelled,
                diagnostics: Vec::new(),
                generated_sources: self.status()?,
                cache_hit: false,
                error: None,
            });
        }

        // If generated sources are globally disabled, short-circuit without invoking build tools.
        if !self.config.generated_sources.enabled {
            progress.event(AptProgressEvent::report(
                "Generated sources are disabled via nova config",
            ));
            progress.event(AptProgressEvent::end());
            return Ok(AptRunResult {
                status: AptRunStatus::UpToDate,
                diagnostics: Vec::new(),
                generated_sources: self.status()?,
                cache_hit: false,
                error: None,
            });
        }

        // Best-effort build metadata extraction: improves generated root discovery and
        // allows us to persist build-tool-specific generated directories for nova-project.
        let gradle_projects = self.gradle_project_paths();
        let _ = self.apply_build_annotation_processing(build, gradle_projects.as_ref());
        let _ = self.write_generated_roots_snapshot();

        // Bazel: require explicit target, but keep the request stable and non-fatal.
        if self.project.build_system == BuildSystem::Bazel {
            let mut diagnostics = Vec::new();
            let mut error = None;
            let status = match target {
                AptRunTarget::BazelTarget(target) => {
                    progress.event(AptProgressEvent::report(format!(
                        "Building Bazel target {target}"
                    )));
                    match build.build_bazel(&self.project.workspace_root, &target) {
                        Ok(result) => {
                            diagnostics.extend(result.diagnostics);
                            AptRunStatus::Ran
                        }
                        Err(err) => {
                            error = Some(err.to_string());
                            AptRunStatus::Failed
                        }
                    }
                }
                AptRunTarget::Workspace => {
                    progress.event(AptProgressEvent::report(
                        "Bazel annotation processing requires an explicit target",
                    ));
                    AptRunStatus::UpToDate
                }
                _ => {
                    progress.event(AptProgressEvent::report(
                        "non-bazel target provided for Bazel project",
                    ));
                    AptRunStatus::Failed
                }
            };

            progress.event(AptProgressEvent::end());
            return Ok(AptRunResult {
                status,
                diagnostics,
                generated_sources: self.status()?,
                cache_hit: false,
                error,
            });
        }

        let mut run_cache = AptRunCache::load(&self.project.workspace_root).ok();
        let mut mtime_cache = AptMtimeCache::load(&self.project.workspace_root).ok();

        let modules = match self.resolve_modules(&target, gradle_projects.as_ref()) {
            Ok(modules) => modules,
            Err(err) => {
                progress.event(AptProgressEvent::report(err.clone()));
                progress.event(AptProgressEvent::end());
                return Ok(AptRunResult {
                    status: AptRunStatus::Failed,
                    diagnostics: Vec::new(),
                    generated_sources: self.status()?,
                    cache_hit: false,
                    error: Some(err),
                });
            }
        };

        let (planned, cache_key) = {
            let mut mtimes: Box<dyn MtimeProvider> = match mtime_cache.as_mut() {
                Some(cache) => Box::new(CachedMtimeProvider { cache }),
                None => Box::new(FsMtimeProvider),
            };
            let mut freshness = FreshnessCalculator::new(&self.project, mtimes.as_mut());

            let mut planned = Vec::new();
            for module in &modules {
                if cancel.as_ref().is_some_and(|token| token.is_cancelled()) {
                    progress.event(AptProgressEvent::end());
                    return Ok(AptRunResult {
                        status: AptRunStatus::Cancelled,
                        diagnostics: Vec::new(),
                        generated_sources: self.status()?,
                        cache_hit: false,
                        error: None,
                    });
                }

                if let Some(plan) = self.plan_module_annotation_processing(
                    module,
                    gradle_projects.as_ref(),
                    &mut freshness,
                )? {
                    planned.push(plan);
                }
            }

            let cache_key = self.run_cache_key(&target, &modules, &mut freshness);
            (planned, cache_key)
        };

        if planned.is_empty() {
            if let Some(cache) = run_cache.as_mut() {
                if let Some(hit) = cache.get(&cache_key) {
                    let result: AptRunResult = hit.into();
                    progress.event(AptProgressEvent::report("Generated sources are up to date"));
                    progress.event(AptProgressEvent::end());
                    let _ = cache.save();
                    if let Some(cache) = mtime_cache.as_mut() {
                        let _ = cache.save();
                    }
                    return Ok(result);
                }
            }

            progress.event(AptProgressEvent::report("Generated sources are up to date"));
            let generated_sources = if let Some(cache) = mtime_cache.as_mut() {
                self.status_cached(cache)?
            } else {
                self.status()?
            };
            progress.event(AptProgressEvent::end());

            let result = AptRunResult {
                status: AptRunStatus::UpToDate,
                diagnostics: Vec::new(),
                generated_sources,
                cache_hit: false,
                error: None,
            };
            if let Some(cache) = run_cache.as_mut() {
                cache.insert(cache_key, result.clone().into());
                let _ = cache.save();
            }
            if let Some(cache) = mtime_cache.as_mut() {
                let _ = cache.save();
            }
            return Ok(result);
        }

        let mut diagnostics = Vec::new();
        let mut error = None;
        let mut cancelled = false;

        for plan in &planned {
            if cancel.as_ref().is_some_and(|token| token.is_cancelled()) {
                cancelled = true;
                break;
            }

            let event =
                AptProgressEvent::report(plan.description()).for_module(&plan.module, plan.kind);
            progress.event(event);

            let result = match (&self.project.build_system, &plan.action) {
                (BuildSystem::Maven, ModuleBuildAction::Maven { module, goal }) => {
                    build.build_maven_goal(&self.project.workspace_root, module.as_deref(), *goal)
                }
                (
                    BuildSystem::Gradle,
                    ModuleBuildAction::Gradle {
                        project_root,
                        project_path,
                        task,
                    },
                ) => build.build_gradle_task(project_root, project_path.as_deref(), *task),
                _ => Ok(BuildResult::default()),
            };

            match result {
                Ok(result) => {
                    diagnostics.extend(result.diagnostics);
                }
                Err(err) => {
                    // Treat IO interruption as cancellation (best-effort).
                    if matches!(&err, BuildError::Io(io_err) if io_err.kind() == io::ErrorKind::Interrupted)
                    {
                        cancelled = true;
                        break;
                    }
                    error = Some(err.to_string());
                    break;
                }
            }
        }

        // Generated roots may have been modified; invalidate cached mtimes under those roots so
        // the post-run status is accurate.
        if let Some(cache) = mtime_cache.as_mut() {
            for module in &modules {
                for root in self.generated_roots_for_module(module) {
                    cache.invalidate_path(&root.path);
                }
            }
        }

        let generated_sources = if let Some(cache) = mtime_cache.as_mut() {
            self.status_cached(cache)?
        } else {
            self.status()?
        };

        let status = if cancelled {
            AptRunStatus::Cancelled
        } else if error.is_some() {
            AptRunStatus::Failed
        } else {
            AptRunStatus::Ran
        };

        progress.event(AptProgressEvent::end());

        let result = AptRunResult {
            status,
            diagnostics,
            generated_sources,
            cache_hit: false,
            error,
        };

        if matches!(result.status, AptRunStatus::Ran | AptRunStatus::UpToDate) {
            if let Some(cache) = run_cache.as_mut() {
                cache.insert(cache_key, result.clone().into());
                let _ = cache.save();
            }
        } else if let Some(cache) = run_cache.as_mut() {
            // Best-effort: still persist access timestamps.
            let _ = cache.save();
        }

        if let Some(cache) = mtime_cache.as_mut() {
            let _ = cache.save();
        }

        Ok(result)
    }

    /// Like [`AptManager::run_annotation_processing_for_target`] but first attempts to populate
    /// per-module annotation processing configuration from the build tool.
    ///
    /// This is best-effort: if build metadata extraction fails, Nova falls back to conventional
    /// generated source roots when deciding which modules are stale.
    pub fn run_annotation_processing_for_target_with_build(
        &mut self,
        build: &BuildManager,
        target: AptRunTarget,
        progress: &mut dyn ProgressReporter,
    ) -> nova_build::Result<BuildResult> {
        let gradle_projects = self.gradle_project_paths();
        let _ = self.apply_build_annotation_processing(build, gradle_projects.as_ref());
        let _ = self.write_generated_roots_snapshot();
        self.run_annotation_processing_for_target(build, target, progress)
    }

    pub fn run_annotation_processing(
        &self,
        build: &BuildManager,
        progress: &mut dyn ProgressReporter,
    ) -> nova_build::Result<BuildResult> {
        self.run_annotation_processing_for_target(build, AptRunTarget::Workspace, progress)
    }

    pub fn run_annotation_processing_for_target(
        &self,
        build: &impl AptBuildExecutor,
        target: AptRunTarget,
        progress: &mut dyn ProgressReporter,
    ) -> nova_build::Result<BuildResult> {
        progress.event(AptProgressEvent::begin("Running annotation processing"));

        if self.project.build_system == BuildSystem::Bazel {
            return match target {
                AptRunTarget::BazelTarget(target) => {
                    progress.event(AptProgressEvent::report(format!(
                        "Building Bazel target {target}"
                    )));
                    let result = build.build_bazel(&self.project.workspace_root, &target)?;
                    progress.event(AptProgressEvent::end());
                    Ok(result)
                }
                AptRunTarget::Workspace => {
                    progress.event(AptProgressEvent::report(
                        "Bazel annotation processing requires an explicit target",
                    ));
                    progress.event(AptProgressEvent::end());
                    Ok(BuildResult {
                        diagnostics: Vec::new(),
                        ..Default::default()
                    })
                }
                _ => Err(BuildError::Unsupported(
                    "non-bazel target provided for Bazel project".to_string(),
                )),
            };
        }

        let gradle_projects = self.gradle_project_paths();
        let mut mtime_provider = FsMtimeProvider;
        let mut freshness = FreshnessCalculator::new(&self.project, &mut mtime_provider);
        let modules = self
            .resolve_modules(&target, gradle_projects.as_ref())
            .map_err(BuildError::Unsupported)?;

        let mut planned = Vec::new();
        for module in modules {
            if let Some(plan) = self
                .plan_module_annotation_processing(module, gradle_projects.as_ref(), &mut freshness)
                .map_err(BuildError::Io)?
            {
                planned.push(plan);
            }
        }

        if planned.is_empty() {
            progress.event(AptProgressEvent::report("Generated sources are up to date"));
            progress.event(AptProgressEvent::end());
            return Ok(BuildResult {
                diagnostics: Vec::new(),
                ..Default::default()
            });
        }

        let mut diagnostics = Vec::new();
        for plan in planned {
            let event =
                AptProgressEvent::report(plan.description()).for_module(&plan.module, plan.kind);
            progress.event(event);

            let result = match (&self.project.build_system, plan.action) {
                (BuildSystem::Maven, ModuleBuildAction::Maven { module, goal }) => {
                    build.build_maven(&self.project.workspace_root, module.as_deref(), goal)?
                }
                (
                    BuildSystem::Gradle,
                    ModuleBuildAction::Gradle {
                        project_root,
                        project_path,
                        task,
                    },
                ) => build.build_gradle(&project_root, project_path.as_deref(), task)?,
                _ => BuildResult {
                    diagnostics: Vec::new(),
                    ..Default::default()
                },
            };
            diagnostics.extend(result.diagnostics);
        }

        progress.event(AptProgressEvent::end());
        Ok(BuildResult {
            diagnostics,
            ..Default::default()
        })
    }
}

fn max_java_mtime(root: &Path) -> io::Result<Option<SystemTime>> {
    let files = core_fs::collect_java_files(root)?;
    core_fs::max_modified_time(files)
}

#[derive(Debug, Clone)]
struct ModuleBuildPlan {
    module: Module,
    kind: SourceRootKind,
    action: ModuleBuildAction,
}

impl ModuleBuildPlan {
    fn description(&self) -> String {
        match self.kind {
            SourceRootKind::Main => format!("Building module {} (main)", self.module.name),
            SourceRootKind::Test => format!("Building module {} (test)", self.module.name),
        }
    }
}

#[derive(Debug, Clone)]
enum ModuleBuildAction {
    Maven {
        module: Option<PathBuf>,
        goal: MavenBuildGoal,
    },
    Gradle {
        /// Gradle build root on disk.
        ///
        /// Normally this is the workspace root, but for composite builds (`includeBuild`) we may
        /// need to invoke Gradle in the included build's root directory.
        project_root: PathBuf,
        project_path: Option<String>,
        task: GradleBuildTask,
    },
}

#[derive(Debug, Clone, Default)]
struct GradleProjectPaths {
    /// Map from Gradle project path (`:app`) -> module root directory.
    by_path: HashMap<String, PathBuf>,
    /// Map from module root directory -> Gradle project path (`:app`).
    by_root: HashMap<PathBuf, String>,
}

#[derive(Debug, Clone)]
struct GradleInvocation {
    project_root: PathBuf,
    project_path: Option<String>,
}

impl AptManager {
    fn gradle_project_paths(&self) -> Option<GradleProjectPaths> {
        if self.project.build_system != BuildSystem::Gradle {
            return None;
        }

        let mut options = LoadOptions::default();
        options.nova_config = self.config.clone();

        let model =
            load_workspace_model_with_options(&self.project.workspace_root, &options).ok()?;
        if model.build_system != BuildSystem::Gradle {
            return None;
        }

        let mut out = GradleProjectPaths::default();
        for module in &model.modules {
            let WorkspaceModuleBuildId::Gradle { project_path } = &module.build_id else {
                continue;
            };
            out.by_path
                .insert(project_path.clone(), module.root.clone());
            out.by_root
                .insert(module.root.clone(), project_path.clone());
        }
        Some(out)
    }

    fn gradle_invocation_for_module_root(
        &self,
        module_root: &Path,
        gradle_projects: Option<&GradleProjectPaths>,
    ) -> Option<GradleInvocation> {
        if self.project.build_system != BuildSystem::Gradle {
            return None;
        }

        let workspace_root = self.project.workspace_root.as_path();

        let mut project_path = gradle_projects
            .and_then(|projects| projects.by_root.get(module_root))
            .cloned();

        if project_path.is_none() {
            project_path = module_root
                .strip_prefix(workspace_root)
                .ok()
                .and_then(rel_to_gradle_project_path);
        }

        let project_path = project_path.as_deref().unwrap_or(":");

        // Resolve Gradle composite builds (`includeBuild(...)`). These are separate Gradle builds,
        // so we need to invoke Gradle from the included build's root directory.
        if let Some(root_path) = included_build_root_project_path(project_path) {
            let projects = gradle_projects?;
            let project_root = projects.by_path.get(root_path)?.clone();
            let nested = project_path.strip_prefix(root_path).unwrap_or("");
            let nested = normalize_gradle_project_path(nested).map(|p| p.into_owned());

            return Some(GradleInvocation {
                project_root,
                project_path: nested,
            });
        }

        let normalized = normalize_gradle_project_path(project_path).map(|p| p.into_owned());
        Some(GradleInvocation {
            project_root: workspace_root.to_path_buf(),
            project_path: normalized,
        })
    }

    fn generated_roots_for_module(&self, module: &Module) -> Vec<SourceRoot> {
        if !self.config.generated_sources.enabled {
            return Vec::new();
        }

        let module_root = &module.root;
        let mut candidates: Vec<(SourceRootKind, PathBuf)> = Vec::new();

        if let Some(override_roots) = &self.config.generated_sources.override_roots {
            for root in override_roots {
                let path = if root.is_absolute() {
                    root.clone()
                } else {
                    module_root.join(root)
                };
                candidates.push((SourceRootKind::Main, path));
            }
        } else {
            match module.annotation_processing.main.as_ref() {
                Some(cfg) if cfg.enabled => match cfg.generated_sources_dir.clone() {
                    Some(dir) => candidates.push((SourceRootKind::Main, dir)),
                    None => push_default_generated_dirs(
                        &mut candidates,
                        self.project.build_system,
                        module_root,
                        SourceRootKind::Main,
                    ),
                },
                Some(_) => {}
                None => push_default_generated_dirs(
                    &mut candidates,
                    self.project.build_system,
                    module_root,
                    SourceRootKind::Main,
                ),
            }

            match module.annotation_processing.test.as_ref() {
                Some(cfg) if cfg.enabled => match cfg.generated_sources_dir.clone() {
                    Some(dir) => candidates.push((SourceRootKind::Test, dir)),
                    None => push_default_generated_dirs(
                        &mut candidates,
                        self.project.build_system,
                        module_root,
                        SourceRootKind::Test,
                    ),
                },
                Some(_) => {}
                None => push_default_generated_dirs(
                    &mut candidates,
                    self.project.build_system,
                    module_root,
                    SourceRootKind::Test,
                ),
            }

            for root in &self.config.generated_sources.additional_roots {
                let path = if root.is_absolute() {
                    root.clone()
                } else {
                    module_root.join(root)
                };
                candidates.push((SourceRootKind::Main, path));
            }
        }

        let mut seen = HashSet::new();
        let mut roots = Vec::new();
        for (kind, path) in candidates {
            if !seen.insert((kind, path.clone())) {
                continue;
            }
            roots.push(SourceRoot {
                kind,
                origin: SourceRootOrigin::Generated,
                path,
            });
        }
        roots.sort_by(|a, b| a.path.cmp(&b.path).then(a.kind.cmp(&b.kind)));
        roots
    }

    fn apply_build_annotation_processing(
        &mut self,
        build: &BuildManager,
        gradle_projects: Option<&GradleProjectPaths>,
    ) -> nova_build::Result<()> {
        let workspace_root = self.project.workspace_root.clone();

        match self.project.build_system {
            BuildSystem::Maven => {
                for module in &mut self.project.modules {
                    let rel = module
                        .root
                        .strip_prefix(&workspace_root)
                        .ok()
                        .filter(|p| !p.as_os_str().is_empty());
                    module.annotation_processing =
                        build.annotation_processing_maven(&workspace_root, rel)?;
                }
            }
            BuildSystem::Gradle => {
                // Compute invocations up-front so we can mutate `self.project.modules` afterwards
                // without borrowing conflicts.
                let invocations: Vec<_> = self
                    .project
                    .modules
                    .iter()
                    .map(|module| {
                        self.gradle_invocation_for_module_root(
                            module.root.as_path(),
                            gradle_projects,
                        )
                    })
                    .collect();

                for (module, invocation) in self.project.modules.iter_mut().zip(invocations) {
                    let Some(invocation) = invocation else {
                        continue;
                    };

                    // If we can't map this module to a Gradle project path (e.g. `includeFlat` or
                    // projectDir overrides that place modules outside the workspace root) avoid
                    // accidentally querying the root project, which would produce misleading
                    // configuration.
                    if invocation.project_root == workspace_root
                        && invocation.project_path.is_none()
                        && module.root != workspace_root
                    {
                        continue;
                    }

                    module.annotation_processing = build.annotation_processing_gradle(
                        &invocation.project_root,
                        invocation.project_path.as_deref(),
                    )?;
                }
            }
            _ => {}
        }

        Ok(())
    }

    fn resolve_modules<'a>(
        &'a self,
        target: &AptRunTarget,
        gradle_projects: Option<&GradleProjectPaths>,
    ) -> Result<Vec<&'a Module>, String> {
        match target {
            AptRunTarget::Workspace => Ok(self.project.modules.iter().collect()),
            AptRunTarget::MavenModule(module_relative) => {
                if self.project.build_system != BuildSystem::Maven {
                    return Err("maven module target provided for non-maven project".to_string());
                }

                let module_root = self.project.workspace_root.join(module_relative);
                let module = self
                    .project
                    .modules
                    .iter()
                    .find(|m| m.root == module_root)
                    .ok_or_else(|| {
                        format!("maven module {} not found", module_relative.display())
                    })?;
                Ok(vec![module])
            }
            AptRunTarget::GradleProject(project_path) => {
                if self.project.build_system != BuildSystem::Gradle {
                    return Err("gradle project target provided for non-gradle project".to_string());
                }

                let Some(project_path) = normalize_gradle_project_path(project_path) else {
                    // Treat empty / `:` as workspace root.
                    return Ok(self.project.modules.iter().collect());
                };

                let module_root = match gradle_projects {
                    Some(projects) => projects
                        .by_path
                        .get(project_path.as_ref())
                        .cloned()
                        .ok_or_else(|| format!("gradle project {project_path} not found"))?,
                    None => self
                        .project
                        .workspace_root
                        .join(gradle_project_path_to_rel(project_path.as_ref())),
                };
                let module = self
                    .project
                    .modules
                    .iter()
                    .find(|m| m.root == module_root)
                    .ok_or_else(|| format!("gradle project {project_path} not found"))?;
                Ok(vec![module])
            }
            AptRunTarget::BazelTarget(_) => {
                if self.project.build_system != BuildSystem::Bazel {
                    return Err("bazel target provided for non-bazel project".to_string());
                }
                Ok(self.project.modules.iter().collect())
            }
        }
    }

    fn plan_module_annotation_processing(
        &self,
        module: &Module,
        gradle_projects: Option<&GradleProjectPaths>,
        freshness: &mut FreshnessCalculator<'_>,
    ) -> io::Result<Option<ModuleBuildPlan>> {
        let generated_roots = self.generated_roots_for_module(module);
        if generated_roots.is_empty() {
            return Ok(None);
        }

        let mut main_stale = false;
        let mut test_stale = false;

        for root in &generated_roots {
            let state = freshness.freshness_for_root(&module.root, root)?;
            if matches!(
                state,
                GeneratedSourcesFreshness::Missing | GeneratedSourcesFreshness::Stale
            ) {
                match root.kind {
                    SourceRootKind::Main => main_stale = true,
                    SourceRootKind::Test => test_stale = true,
                }
            }
        }

        if !main_stale && !test_stale {
            return Ok(None);
        }

        let (kind, action) = match self.project.build_system {
            BuildSystem::Maven => {
                let rel = module
                    .root
                    .strip_prefix(&self.project.workspace_root)
                    .ok()
                    .filter(|p| !p.as_os_str().is_empty())
                    .map(|p| p.to_path_buf());
                if test_stale {
                    (
                        SourceRootKind::Test,
                        ModuleBuildAction::Maven {
                            module: rel,
                            goal: MavenBuildGoal::TestCompile,
                        },
                    )
                } else {
                    (
                        SourceRootKind::Main,
                        ModuleBuildAction::Maven {
                            module: rel,
                            goal: MavenBuildGoal::Compile,
                        },
                    )
                }
            }
            BuildSystem::Gradle => {
                let Some(invocation) =
                    self.gradle_invocation_for_module_root(module.root.as_path(), gradle_projects)
                else {
                    return Ok(None);
                };

                // If we can't map this module to a concrete Gradle project path, avoid running the
                // root build as a fallback for non-root modules (it would produce confusing
                // results without actually generating sources for the intended module).
                if invocation.project_root == self.project.workspace_root
                    && invocation.project_path.is_none()
                    && module.root != self.project.workspace_root
                {
                    return Ok(None);
                }

                let (kind, task) = if test_stale {
                    (SourceRootKind::Test, GradleBuildTask::CompileTestJava)
                } else {
                    (SourceRootKind::Main, GradleBuildTask::CompileJava)
                };
                (
                    kind,
                    ModuleBuildAction::Gradle {
                        project_root: invocation.project_root,
                        project_path: invocation.project_path,
                        task,
                    },
                )
            }
            BuildSystem::Bazel => return Ok(None),
            BuildSystem::Simple => return Ok(None),
        };

        Ok(Some(ModuleBuildPlan {
            module: module.clone(),
            kind,
            action,
        }))
    }
}

fn gradle_project_path_to_rel(project_path: &str) -> PathBuf {
    let trimmed = project_path.trim_matches(':');
    let mut rel = PathBuf::new();
    for part in trimmed.split(':').filter(|p| !p.is_empty()) {
        rel.push(part);
    }
    rel
}

fn rel_to_gradle_project_path(rel: &Path) -> Option<String> {
    if rel.as_os_str().is_empty() {
        return None;
    }
    let mut s = String::new();
    for component in rel.components() {
        let part = component.as_os_str().to_string_lossy();
        if part.is_empty() || part == "." {
            continue;
        }
        s.push(':');
        s.push_str(&part);
    }
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn included_build_root_project_path(project_path: &str) -> Option<&str> {
    if !project_path.starts_with(":__includedBuild_") {
        return None;
    }

    // Included build project paths are synthesized as:
    // - `:__includedBuild_<name>` for the included build root, and
    // - `:__includedBuild_<name>:subproject` for included build subprojects.
    let rest = project_path.strip_prefix(':').unwrap_or(project_path);
    match rest.find(':') {
        Some(idx) => Some(&project_path[..idx + 1]),
        None => Some(project_path),
    }
}

fn normalize_gradle_project_path(project_path: &str) -> Option<Cow<'_, str>> {
    let project_path = project_path.trim();
    if project_path.is_empty() || project_path == ":" {
        return None;
    }

    if project_path.starts_with(':') {
        Some(Cow::Borrowed(project_path))
    } else {
        Some(Cow::Owned(format!(":{project_path}")))
    }
}

fn push_default_generated_dirs(
    out: &mut Vec<(SourceRootKind, PathBuf)>,
    build_system: BuildSystem,
    module_root: &Path,
    kind: SourceRootKind,
) {
    match (build_system, kind) {
        (BuildSystem::Maven, SourceRootKind::Main) => out.push((
            SourceRootKind::Main,
            module_root.join("target/generated-sources/annotations"),
        )),
        (BuildSystem::Maven, SourceRootKind::Test) => out.push((
            SourceRootKind::Test,
            module_root.join("target/generated-test-sources/test-annotations"),
        )),
        (BuildSystem::Gradle, SourceRootKind::Main) => out.push((
            SourceRootKind::Main,
            module_root.join("build/generated/sources/annotationProcessor/java/main"),
        )),
        (BuildSystem::Gradle, SourceRootKind::Test) => out.push((
            SourceRootKind::Test,
            module_root.join("build/generated/sources/annotationProcessor/java/test"),
        )),
        (BuildSystem::Simple, SourceRootKind::Main) => {
            out.push((
                SourceRootKind::Main,
                module_root.join("target/generated-sources/annotations"),
            ));
            out.push((
                SourceRootKind::Main,
                module_root.join("build/generated/sources/annotationProcessor/java/main"),
            ));
        }
        (BuildSystem::Simple, SourceRootKind::Test) => {
            out.push((
                SourceRootKind::Test,
                module_root.join("target/generated-test-sources/test-annotations"),
            ));
            out.push((
                SourceRootKind::Test,
                module_root.join("build/generated/sources/annotationProcessor/java/test"),
            ));
        }
        (BuildSystem::Bazel, _) => {}
    }
}

#[cfg(test)]
mod tests {
    use nova_build::BuildManager;
    use nova_config::NovaConfig;
    use nova_core::{FileId, Name};
    use nova_hir::queries::HirDatabase;
    use nova_index::ClassIndex;
    use nova_jdk::JdkIndex;
    use nova_project::{
        load_project_with_options, BuildSystem, JavaConfig, LoadOptions, Module, ProjectConfig,
        SourceRootOrigin,
    };
    use nova_resolve::{build_scopes, Resolver};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use tempfile::TempDir;

    #[derive(Default)]
    struct TestDb {
        files: std::collections::HashMap<FileId, Arc<str>>,
    }

    impl TestDb {
        fn set_file_text(&mut self, file: FileId, text: impl Into<Arc<str>>) {
            self.files.insert(file, text.into());
        }
    }

    impl HirDatabase for TestDb {
        fn file_text(&self, file: FileId) -> Arc<str> {
            self.files
                .get(&file)
                .cloned()
                .unwrap_or_else(|| Arc::from(""))
        }
    }

    fn fixture_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/maven_simple")
    }

    fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let ty = entry.file_type()?;
            let dst_path = dst.join(entry.file_name());
            if ty.is_dir() {
                copy_dir_recursive(&entry.path(), &dst_path)?;
            } else if ty.is_file() {
                std::fs::copy(entry.path(), dst_path)?;
            }
        }
        Ok(())
    }

    fn write_generated_hello(project_root: &Path) {
        let path = project_root
            .join("target/generated-sources/annotations/com/example/generated/GeneratedHello.java");
        std::fs::create_dir_all(path.parent().expect("generated file parent")).unwrap();
        std::fs::write(
            &path,
            r#"
package com.example.generated;

public class GeneratedHello {
    public static String hello() {
        return "hello";
    }
}
"#,
        )
        .unwrap();
    }

    fn temp_project_root_with_generated_hello() -> TempDir {
        let dir = TempDir::new().unwrap();
        copy_dir_recursive(&fixture_root().join("src"), &dir.path().join("src")).unwrap();
        write_generated_hello(dir.path());
        dir
    }

    fn write_java_source(dir: &Path, name: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(name), "class App {}".as_bytes()).unwrap();
    }

    #[test]
    fn gradle_project_path_mapping_respects_project_dir_override() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("settings.gradle"),
            "include ':app'\nproject(':app').projectDir = file('modules/application')\n",
        )
        .unwrap();

        let app_root = dir.path().join("modules/application");
        write_java_source(&app_root.join("src/main/java"), "App.java");

        let config = NovaConfig::default();
        let mut options = LoadOptions::default();
        options.nova_config = config.clone();
        let project = load_project_with_options(dir.path(), &options).unwrap();
        assert_eq!(project.build_system, BuildSystem::Gradle);

        let apt = crate::AptManager::new(project, config);
        let gradle_projects = apt
            .gradle_project_paths()
            .expect("workspace model should load");

        let modules = apt
            .resolve_modules(
                &crate::AptRunTarget::GradleProject(":app".to_string()),
                Some(&gradle_projects),
            )
            .unwrap();
        assert_eq!(modules.len(), 1);
        assert_eq!(modules[0].root, app_root.canonicalize().unwrap());

        let mut mtime_provider = super::FsMtimeProvider;
        let mut freshness = super::FreshnessCalculator::new(apt.project(), &mut mtime_provider);
        let plan = apt
            .plan_module_annotation_processing(modules[0], Some(&gradle_projects), &mut freshness)
            .unwrap()
            .expect("expected a build plan due to missing generated roots");

        match plan.action {
            super::ModuleBuildAction::Gradle {
                project_root,
                project_path,
                task,
            } => {
                assert_eq!(project_root, apt.project().workspace_root);
                assert_eq!(project_path.as_deref(), Some(":app"));
                assert_eq!(task, nova_build::GradleBuildTask::CompileJava);
            }
            other => panic!("expected Gradle build action, got {other:?}"),
        }
    }

    #[test]
    fn gradle_project_path_mapping_respects_include_flat_outside_workspace_root() {
        let dir = tempfile::tempdir().unwrap();
        let workspace_root = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace_root).unwrap();
        std::fs::write(
            workspace_root.join("settings.gradle"),
            "includeFlat 'app'\n",
        )
        .unwrap();

        let app_root = dir.path().join("app");
        write_java_source(&app_root.join("src/main/java"), "App.java");

        let config = NovaConfig::default();
        let mut options = LoadOptions::default();
        options.nova_config = config.clone();
        let project = load_project_with_options(&workspace_root, &options).unwrap();
        assert_eq!(project.build_system, BuildSystem::Gradle);

        let apt = crate::AptManager::new(project, config);
        let gradle_projects = apt
            .gradle_project_paths()
            .expect("workspace model should load");

        let modules = apt
            .resolve_modules(
                &crate::AptRunTarget::GradleProject(":app".to_string()),
                Some(&gradle_projects),
            )
            .unwrap();
        assert_eq!(modules.len(), 1);
        assert_eq!(modules[0].root, app_root.canonicalize().unwrap());
        assert!(
            !modules[0].root.starts_with(&apt.project().workspace_root),
            "expected includeFlat module root to live outside the workspace root"
        );

        let mut mtime_provider = super::FsMtimeProvider;
        let mut freshness = super::FreshnessCalculator::new(apt.project(), &mut mtime_provider);
        let plan = apt
            .plan_module_annotation_processing(modules[0], Some(&gradle_projects), &mut freshness)
            .unwrap()
            .expect("expected a build plan due to missing generated roots");

        match plan.action {
            super::ModuleBuildAction::Gradle {
                project_root,
                project_path,
                task,
            } => {
                assert_eq!(project_root, apt.project().workspace_root);
                assert_eq!(project_path.as_deref(), Some(":app"));
                assert_eq!(task, nova_build::GradleBuildTask::CompileJava);
            }
            other => panic!("expected Gradle build action, got {other:?}"),
        }
    }

    #[test]
    fn gradle_project_path_mapping_invokes_included_builds_from_their_root() {
        let dir = tempfile::tempdir().unwrap();
        let workspace_root = dir.path();
        std::fs::write(
            workspace_root.join("settings.gradle"),
            "includeBuild 'build-logic'\n",
        )
        .unwrap();

        let build_logic_root = workspace_root.join("build-logic");
        std::fs::create_dir_all(&build_logic_root).unwrap();
        std::fs::write(
            build_logic_root.join("settings.gradle"),
            "include ':conventions'\n",
        )
        .unwrap();

        let conventions_root = build_logic_root.join("conventions");
        write_java_source(&conventions_root.join("src/main/java"), "Conventions.java");

        let config = NovaConfig::default();
        let mut options = LoadOptions::default();
        options.nova_config = config.clone();
        let project = load_project_with_options(workspace_root, &options).unwrap();
        assert_eq!(project.build_system, BuildSystem::Gradle);

        let apt = crate::AptManager::new(project, config);
        let gradle_projects = apt
            .gradle_project_paths()
            .expect("workspace model should load");

        let conventions_root = conventions_root.canonicalize().unwrap();
        let build_logic_root = build_logic_root.canonicalize().unwrap();
        let module = apt
            .project()
            .modules
            .iter()
            .find(|m| m.root == conventions_root)
            .expect("expected included build subproject module to be present");

        let mut mtime_provider = super::FsMtimeProvider;
        let mut freshness = super::FreshnessCalculator::new(apt.project(), &mut mtime_provider);
        let plan = apt
            .plan_module_annotation_processing(module, Some(&gradle_projects), &mut freshness)
            .unwrap()
            .expect("expected a build plan due to missing generated roots");

        match plan.action {
            super::ModuleBuildAction::Gradle {
                project_root,
                project_path,
                task,
            } => {
                assert_eq!(project_root, build_logic_root);
                assert_eq!(project_path.as_deref(), Some(":conventions"));
                assert_eq!(task, nova_build::GradleBuildTask::CompileJava);
            }
            other => panic!("expected Gradle build action, got {other:?}"),
        }
    }

    #[test]
    fn resolves_generated_type_when_generated_roots_enabled() {
        let dir = temp_project_root_with_generated_hello();
        let project_root = dir.path();

        let config = NovaConfig::default();
        let mut options = LoadOptions::default();
        options.nova_config = config.clone();
        let project = load_project_with_options(project_root, &options).unwrap();

        let generated_root = project
            .workspace_root
            .join("target/generated-sources/annotations");
        assert!(project
            .source_roots
            .iter()
            .any(|sr| { sr.origin == SourceRootOrigin::Generated && sr.path == generated_root }));

        let index = ClassIndex::build(&project.source_roots).unwrap();
        assert!(index.contains("com.example.generated.GeneratedHello"));
        let location = index
            .lookup("com.example.generated.GeneratedHello")
            .expect("generated class location");
        assert_eq!(location.origin, SourceRootOrigin::Generated);
        assert_eq!(location.source_root, generated_root);

        let file = FileId::from_raw(0);
        let mut db = TestDb::default();
        db.set_file_text(
            file,
            r#"
package com.example.app;
import com.example.generated.GeneratedHello;
class C {}
"#,
        );

        let jdk = JdkIndex::new();
        let resolver = Resolver::new(&jdk).with_classpath(&index);
        let scopes = build_scopes(&db, file);
        let resolved = resolver.resolve_name(
            &scopes.scopes,
            scopes.file_scope,
            &Name::from("GeneratedHello"),
        );

        assert!(resolved.is_some());
    }

    #[test]
    fn does_not_resolve_generated_type_when_generated_roots_excluded() {
        let dir = temp_project_root_with_generated_hello();
        let project_root = dir.path();

        let mut config = NovaConfig::default();
        config.generated_sources.enabled = false;
        let mut options = LoadOptions::default();
        options.nova_config = config;
        let project = load_project_with_options(project_root, &options).unwrap();

        let generated_root = project
            .workspace_root
            .join("target/generated-sources/annotations");
        assert!(!project
            .source_roots
            .iter()
            .any(|sr| sr.origin == SourceRootOrigin::Generated));
        assert!(!project
            .source_roots
            .iter()
            .any(|sr| sr.path == generated_root));

        let index = ClassIndex::build(&project.source_roots).unwrap();
        assert!(!index.contains("com.example.generated.GeneratedHello"));

        let file = FileId::from_raw(0);
        let mut db = TestDb::default();
        db.set_file_text(
            file,
            r#"
package com.example.app;
import com.example.generated.GeneratedHello;
class C {}
"#,
        );

        let jdk = JdkIndex::new();
        let resolver = Resolver::new(&jdk).with_classpath(&index);
        let scopes = build_scopes(&db, file);
        let resolved = resolver.resolve_name(
            &scopes.scopes,
            scopes.file_scope,
            &Name::from("GeneratedHello"),
        );

        assert!(resolved.is_none());
    }

    #[derive(Debug)]
    struct ErrorRunner;

    impl nova_build::CommandRunner for ErrorRunner {
        fn run(
            &self,
            _cwd: &Path,
            _program: &Path,
            _args: &[String],
        ) -> std::io::Result<nova_build::CommandOutput> {
            Err(std::io::Error::new(std::io::ErrorKind::Other, "boom"))
        }
    }

    #[test]
    fn status_with_build_surfaces_build_metadata_errors() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        std::fs::write(
            root.join("pom.xml"),
            r#"<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>demo</artifactId>
  <version>1.0-SNAPSHOT</version>
</project>
"#,
        )
        .unwrap();

        let project = ProjectConfig {
            workspace_root: root.to_path_buf(),
            build_system: BuildSystem::Maven,
            java: JavaConfig::default(),
            modules: vec![Module {
                name: "root".to_string(),
                root: root.to_path_buf(),
                annotation_processing: Default::default(),
            }],
            jpms_modules: Vec::new(),
            jpms_workspace: None,
            source_roots: Vec::new(),
            module_path: Vec::new(),
            classpath: Vec::new(),
            output_dirs: Vec::new(),
            dependencies: Vec::new(),
            workspace_model: None,
        };

        let build = BuildManager::with_runner(
            root.join(".nova").join("build-cache"),
            Arc::new(ErrorRunner),
        );

        let mut apt = crate::AptManager::new(project, NovaConfig::default());
        let result = apt.status_with_build(&build).unwrap();

        assert!(
            result
                .build_metadata_error
                .as_deref()
                .unwrap_or_default()
                .contains("boom"),
            "expected build_metadata_error to include the runner failure: {result:?}"
        );
        assert_eq!(result.status.modules.len(), 1);
    }
}
