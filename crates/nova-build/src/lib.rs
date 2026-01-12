//! Build tool integration (Maven/Gradle) for classpaths and build diagnostics.
//!
//! LSP does not define how a language server should obtain an accurate
//! classpath nor how it should surface build-tool diagnostics. This crate
//! implements that missing piece for Nova.

mod cache;
mod command;
mod gradle;
mod javac;
mod jpms;
mod maven;
mod module_graph;
mod orchestrator;

pub use cache::{BuildCache, BuildFileFingerprint};
pub use command::{CommandOutput, CommandRunner, DefaultCommandRunner};
pub use gradle::{
    collect_gradle_build_files, parse_gradle_annotation_processing_output,
    parse_gradle_classpath_output, parse_gradle_projects_output, GradleBuild, GradleConfig,
    GradleProjectInfo,
};
pub use javac::{parse_javac_diagnostics, JavacDiagnosticFormat};
pub use maven::{
    collect_maven_build_files, maven_jar_path, parse_maven_classpath_output,
    parse_maven_effective_pom_annotation_processing,
    parse_maven_effective_pom_annotation_processing_with_repo, parse_maven_evaluate_scalar_output,
    MavenBuild, MavenConfig,
};
pub use module_graph::{infer_module_graph, ModuleBuildInfo, ModuleGraph, ModuleId};
pub use orchestrator::{
    BuildDiagnosticsSnapshot, BuildOrchestrator, BuildRequest, BuildStatusSnapshot, BuildTaskId,
    BuildTaskState, CommandRunnerFactory, DefaultCommandRunnerFactory,
};

// Build-system abstraction (see `instructions/build-systems.md`).
pub use nova_build_model::BuildSystemBackend as BuildSystem;

use nova_build_model::AnnotationProcessing;
use nova_core::Diagnostic;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum BuildError {
    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(
        "{tool} command `{command}` failed with exit code {code:?} (output_truncated={output_truncated})\nstdout:\n{stdout}\nstderr:\n{stderr}"
    )]
    CommandFailed {
        tool: &'static str,
        command: String,
        code: Option<i32>,
        stdout: String,
        stderr: String,
        output_truncated: bool,
    },

    #[error("failed to parse build output: {0}")]
    Parse(String),

    #[error(transparent)]
    Cache(#[from] cache::CacheError),

    #[error("unsupported project layout: {0}")]
    Unsupported(String),
}

pub type Result<T> = std::result::Result<T, BuildError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum BuildSystemKind {
    Maven,
    Gradle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MavenBuildGoal {
    Compile,
    TestCompile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GradleBuildTask {
    CompileJava,
    CompileTestJava,
}

/// A resolved compile classpath.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Classpath {
    pub entries: Vec<PathBuf>,
}

impl Classpath {
    pub fn new(entries: Vec<PathBuf>) -> Self {
        Self { entries }
    }
}

/// Java compile + test configuration for a single build module.
///
/// This is intended to be sufficiently complete for IDE-like use cases (classpath,
/// source roots, output directories, language level) while still being cheap to
/// compute via build tool integration.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub struct JavaCompileConfig {
    pub compile_classpath: Vec<PathBuf>,
    pub test_classpath: Vec<PathBuf>,
    /// Best-effort module path. When unavailable, this will be empty.
    pub module_path: Vec<PathBuf>,
    pub main_source_roots: Vec<PathBuf>,
    pub test_source_roots: Vec<PathBuf>,
    pub main_output_dir: Option<PathBuf>,
    pub test_output_dir: Option<PathBuf>,
    pub source: Option<String>,
    pub target: Option<String>,
    pub release: Option<String>,
    /// Best-effort preview flag (e.g. `--enable-preview`).
    pub enable_preview: bool,
}

impl JavaCompileConfig {
    /// Best-effort union of multiple module configurations.
    ///
    /// Useful for multi-module Maven projects where the root POM is an aggregator
    /// (`<packaging>pom</packaging>`) and does not itself represent a compilable
    /// module. Callers that need per-module correctness should prefer the
    /// per-module configs.
    pub fn union(configs: impl IntoIterator<Item = JavaCompileConfig>) -> JavaCompileConfig {
        let mut iter = configs.into_iter();
        let Some(first) = iter.next() else {
            return JavaCompileConfig::default();
        };

        let mut out = first;
        let mut output_dir_candidate = out.main_output_dir.clone();
        let mut test_output_dir_candidate = out.test_output_dir.clone();
        let mut source_candidate = out.source.clone();
        let mut target_candidate = out.target.clone();
        let mut release_candidate = out.release.clone();

        let mut seen_compile = std::collections::HashSet::new();
        let mut seen_test = std::collections::HashSet::new();
        let mut seen_module = std::collections::HashSet::new();
        let mut seen_main_src = std::collections::HashSet::new();
        let mut seen_test_src = std::collections::HashSet::new();

        out.compile_classpath
            .retain(|p| seen_compile.insert(p.clone()));
        out.test_classpath.retain(|p| seen_test.insert(p.clone()));
        out.module_path.retain(|p| seen_module.insert(p.clone()));
        out.main_source_roots
            .retain(|p| seen_main_src.insert(p.clone()));
        out.test_source_roots
            .retain(|p| seen_test_src.insert(p.clone()));

        for cfg in iter {
            for p in cfg.compile_classpath {
                if seen_compile.insert(p.clone()) {
                    out.compile_classpath.push(p);
                }
            }
            for p in cfg.test_classpath {
                if seen_test.insert(p.clone()) {
                    out.test_classpath.push(p);
                }
            }
            for p in cfg.module_path {
                if seen_module.insert(p.clone()) {
                    out.module_path.push(p);
                }
            }
            for p in cfg.main_source_roots {
                if seen_main_src.insert(p.clone()) {
                    out.main_source_roots.push(p);
                }
            }
            for p in cfg.test_source_roots {
                if seen_test_src.insert(p.clone()) {
                    out.test_source_roots.push(p);
                }
            }

            if output_dir_candidate != cfg.main_output_dir {
                output_dir_candidate = None;
            }
            if test_output_dir_candidate != cfg.test_output_dir {
                test_output_dir_candidate = None;
            }
            if source_candidate != cfg.source {
                source_candidate = None;
            }
            if target_candidate != cfg.target {
                target_candidate = None;
            }
            if release_candidate != cfg.release {
                release_candidate = None;
            }

            out.enable_preview |= cfg.enable_preview;
        }

        out.main_output_dir = output_dir_candidate;
        out.test_output_dir = test_output_dir_candidate;
        out.source = source_candidate;
        out.target = target_candidate;
        out.release = release_candidate;
        out
    }
}

/// Summary of a build invocation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BuildResult {
    pub diagnostics: Vec<Diagnostic>,
    /// Best-effort build tool identifier (e.g. `maven`, `gradle`).
    pub tool: Option<String>,
    /// Best-effort rendered command line for the build invocation.
    pub command: Option<String>,
    /// Exit code reported by the build tool (when available).
    pub exit_code: Option<i32>,
    /// Captured stdout from the build tool invocation (bounded).
    pub stdout: String,
    /// Captured stderr from the build tool invocation (bounded).
    pub stderr: String,
    /// Indicates stdout/stderr were truncated due to bounded output capture.
    pub output_truncated: bool,
}

/// High-level entry point for build integration.
#[derive(Debug)]
pub struct BuildManager {
    cache: BuildCache,
    maven: MavenBuild,
    gradle: GradleBuild,
}

impl BuildManager {
    pub fn new(cache_dir: impl Into<PathBuf>) -> Self {
        Self::with_runner(cache_dir, Arc::new(DefaultCommandRunner::default()))
    }

    pub fn with_configs(
        cache_dir: impl Into<PathBuf>,
        maven: MavenConfig,
        gradle: GradleConfig,
    ) -> Self {
        Self::with_configs_and_runner(
            cache_dir,
            maven,
            gradle,
            Arc::new(DefaultCommandRunner::default()),
        )
    }

    pub fn with_runner(cache_dir: impl Into<PathBuf>, runner: Arc<dyn CommandRunner>) -> Self {
        Self::with_configs_and_runner(
            cache_dir,
            MavenConfig::default(),
            GradleConfig::default(),
            runner,
        )
    }

    pub fn with_configs_and_runner(
        cache_dir: impl Into<PathBuf>,
        maven: MavenConfig,
        gradle: GradleConfig,
        runner: Arc<dyn CommandRunner>,
    ) -> Self {
        let cache = BuildCache::new(cache_dir);
        Self {
            cache,
            maven: MavenBuild::with_runner(maven, runner.clone()),
            gradle: GradleBuild::with_runner(gradle, runner),
        }
    }

    pub fn reload_project(&self, project_root: &Path) -> Result<()> {
        self.cache.invalidate_project(project_root)?;
        Ok(())
    }

    pub fn classpath_maven(
        &self,
        project_root: &Path,
        module_relative: Option<&Path>,
    ) -> Result<Classpath> {
        self.maven
            .classpath(project_root, module_relative, &self.cache)
    }

    pub fn build_maven(
        &self,
        project_root: &Path,
        module_relative: Option<&Path>,
    ) -> Result<BuildResult> {
        self.build_maven_goal(project_root, module_relative, MavenBuildGoal::Compile)
    }

    pub fn build_maven_goal(
        &self,
        project_root: &Path,
        module_relative: Option<&Path>,
        goal: MavenBuildGoal,
    ) -> Result<BuildResult> {
        self.maven
            .build_with_goal(project_root, module_relative, goal, &self.cache)
    }

    pub fn java_compile_config_maven(
        &self,
        project_root: &Path,
        module_relative: Option<&Path>,
    ) -> Result<JavaCompileConfig> {
        self.maven
            .java_compile_config(project_root, module_relative, &self.cache)
    }

    pub fn java_compile_config_gradle(
        &self,
        project_root: &Path,
        project_path: Option<&str>,
    ) -> Result<JavaCompileConfig> {
        self.gradle
            .java_compile_config(project_root, project_path, &self.cache)
    }

    pub fn java_compile_configs_all_gradle(
        &self,
        project_root: &Path,
    ) -> Result<Vec<(String, JavaCompileConfig)>> {
        self.gradle
            .java_compile_configs_all(project_root, &self.cache)
    }

    pub fn classpath_gradle(
        &self,
        project_root: &Path,
        project_path: Option<&str>,
    ) -> Result<Classpath> {
        self.gradle
            .classpath(project_root, project_path, &self.cache)
    }

    pub fn build_gradle(
        &self,
        project_root: &Path,
        project_path: Option<&str>,
    ) -> Result<BuildResult> {
        self.build_gradle_task(project_root, project_path, GradleBuildTask::CompileJava)
    }

    pub fn build_gradle_task(
        &self,
        project_root: &Path,
        project_path: Option<&str>,
        task: GradleBuildTask,
    ) -> Result<BuildResult> {
        self.gradle
            .build_with_task(project_root, project_path, task, &self.cache)
    }

    pub fn annotation_processing_maven(
        &self,
        project_root: &Path,
        module_relative: Option<&Path>,
    ) -> Result<AnnotationProcessing> {
        self.maven
            .annotation_processing(project_root, module_relative, &self.cache)
    }

    pub fn annotation_processing_gradle(
        &self,
        project_root: &Path,
        project_path: Option<&str>,
    ) -> Result<AnnotationProcessing> {
        self.gradle
            .annotation_processing(project_root, project_path, &self.cache)
    }
}
