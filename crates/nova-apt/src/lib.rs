use nova_build::{BuildError, BuildManager, BuildResult, GradleBuildTask, MavenBuildGoal};
use nova_config::NovaConfig;
use nova_core::fs as core_fs;
use nova_project::{
    BuildSystem, Module, ProjectConfig, SourceRoot, SourceRootKind, SourceRootOrigin,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Discover generated Java source roots produced by common annotation processor setups.
///
/// This helper exists for components that only know the workspace root on disk
/// (e.g. lightweight navigation/analysis in fixture tests). When a full
/// [`ProjectConfig`] is available, prefer using its generated [`SourceRoot`]s
/// (origin = `Generated`).
pub fn discover_generated_source_roots(project_root: &Path) -> Vec<PathBuf> {
    let candidates = [
        // Maven
        "target/generated-sources/annotations",
        "target/generated-test-sources/test-annotations",
        // Gradle
        "build/generated/sources/annotationProcessor/java/main",
        "build/generated/sources/annotationProcessor/java/test",
        "build/generated/sources/annotationProcessor/java/integrationTest",
    ];

    candidates
        .into_iter()
        .map(|rel| project_root.join(rel))
        .filter(|path| path.is_dir())
        .collect()
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AptRunTarget {
    Workspace,
    MavenModule(PathBuf),
    GradleProject(String),
    BazelTarget(String),
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
        let mut cmd = Command::new("bazel");
        cmd.current_dir(project_root);
        cmd.arg("build").arg(target);
        let output = cmd.output()?;
        if output.status.success() {
            return Ok(BuildResult {
                diagnostics: Vec::new(),
            });
        }

        Err(BuildError::CommandFailed {
            tool: "bazel",
            command: format!("bazel build {target}"),
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
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

static MTIME_CACHE_TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

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

fn open_unique_tmp_file(dest: &Path, parent: &Path) -> io::Result<(PathBuf, std::fs::File)> {
    let file_name = dest
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "destination path has no file name"))?;
    let pid = std::process::id();

    loop {
        let counter = MTIME_CACHE_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
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
                .generated_roots_for_module(&module.root)
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
                .generated_roots_for_module(&module.root)
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
                    })
                }
                _ => Err(BuildError::Unsupported(
                    "non-bazel target provided for Bazel project".to_string(),
                )),
            };
        }

        let mut mtime_provider = FsMtimeProvider;
        let mut freshness = FreshnessCalculator::new(&self.project, &mut mtime_provider);
        let modules = self
            .resolve_modules(&target)
            .map_err(BuildError::Unsupported)?;

        let mut planned = Vec::new();
        for module in modules {
            if let Some(plan) = self
                .plan_module_annotation_processing(module, &mut freshness)
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
                (BuildSystem::Gradle, ModuleBuildAction::Gradle { project_path, task }) => build
                    .build_gradle(&self.project.workspace_root, project_path.as_deref(), task)?,
                _ => BuildResult {
                    diagnostics: Vec::new(),
                },
            };
            diagnostics.extend(result.diagnostics);
        }

        progress.event(AptProgressEvent::end());
        Ok(BuildResult { diagnostics })
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
        project_path: Option<String>,
        task: GradleBuildTask,
    },
}

impl AptManager {
    fn generated_roots_for_module(&self, module_root: &Path) -> Vec<SourceRoot> {
        let mut roots: Vec<_> = self
            .project
            .source_roots
            .iter()
            .filter(|root| root.origin == SourceRootOrigin::Generated)
            .filter(|root| root.path.starts_with(module_root))
            .cloned()
            .collect();
        roots.sort_by(|a, b| a.path.cmp(&b.path).then(a.kind.cmp(&b.kind)));
        roots
    }

    fn resolve_modules<'a>(&'a self, target: &AptRunTarget) -> Result<Vec<&'a Module>, String> {
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

                let module_root = self
                    .project
                    .workspace_root
                    .join(gradle_project_path_to_rel(project_path));
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
        freshness: &mut FreshnessCalculator<'_>,
    ) -> io::Result<Option<ModuleBuildPlan>> {
        let generated_roots = self.generated_roots_for_module(&module.root);
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
                let project_path = module
                    .root
                    .strip_prefix(&self.project.workspace_root)
                    .ok()
                    .and_then(|rel| rel_to_gradle_project_path(rel));
                if test_stale {
                    (
                        SourceRootKind::Test,
                        ModuleBuildAction::Gradle {
                            project_path,
                            task: GradleBuildTask::CompileTestJava,
                        },
                    )
                } else {
                    (
                        SourceRootKind::Main,
                        ModuleBuildAction::Gradle {
                            project_path,
                            task: GradleBuildTask::CompileJava,
                        },
                    )
                }
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

#[cfg(test)]
mod tests {
    use nova_config::NovaConfig;
    use nova_core::{Name, PackageName, QualifiedName};
    use nova_hir::{CompilationUnit, ImportDecl};
    use nova_index::ClassIndex;
    use nova_jdk::JdkIndex;
    use nova_project::{load_project_with_options, LoadOptions, SourceRootOrigin};
    use nova_resolve::Resolver;
    use std::path::PathBuf;

    fn fixture_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/maven_simple")
    }

    #[test]
    fn resolves_generated_type_when_generated_roots_enabled() {
        let project_root = fixture_root();

        let config = NovaConfig::default();
        let mut options = LoadOptions::default();
        options.nova_config = config.clone();
        let project = load_project_with_options(&project_root, &options).unwrap();

        assert!(project
            .source_roots
            .iter()
            .any(|sr| sr.origin == SourceRootOrigin::Generated));

        let index = ClassIndex::build(&project.source_roots).unwrap();
        assert!(index.contains("com.example.generated.GeneratedHello"));

        let mut unit = CompilationUnit::new(Some(PackageName::from_dotted("com.example.app")));
        unit.imports.push(ImportDecl::TypeSingle {
            ty: QualifiedName::from_dotted("com.example.generated.GeneratedHello"),
            alias: None,
        });

        let jdk = JdkIndex::new();
        let resolver = Resolver::new(&jdk).with_classpath(&index);
        let resolved = resolver.resolve_import(&unit, &Name::from("GeneratedHello"));

        assert!(resolved.is_some());
    }

    #[test]
    fn does_not_resolve_generated_type_when_generated_roots_excluded() {
        let project_root = fixture_root();

        let mut config = NovaConfig::default();
        config.generated_sources.enabled = false;
        let mut options = LoadOptions::default();
        options.nova_config = config;
        let project = load_project_with_options(&project_root, &options).unwrap();

        assert!(!project
            .source_roots
            .iter()
            .any(|sr| sr.origin == SourceRootOrigin::Generated));

        let index = ClassIndex::build(&project.source_roots).unwrap();
        assert!(!index.contains("com.example.generated.GeneratedHello"));

        let mut unit = CompilationUnit::new(Some(PackageName::from_dotted("com.example.app")));
        unit.imports.push(ImportDecl::TypeSingle {
            ty: QualifiedName::from_dotted("com.example.generated.GeneratedHello"),
            alias: None,
        });

        let jdk = JdkIndex::new();
        let resolver = Resolver::new(&jdk).with_classpath(&index);
        let resolved = resolver.resolve_import(&unit, &Name::from("GeneratedHello"));

        assert!(resolved.is_none());
    }
}
