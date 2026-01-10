//! Build tool integration (Maven/Gradle) for classpaths and build diagnostics.
//!
//! LSP does not define how a language server should obtain an accurate
//! classpath nor how it should surface build-tool diagnostics. This crate
//! implements that missing piece for Nova.

mod cache;
mod gradle;
mod javac;
mod maven;

pub use cache::{BuildCache, BuildFileFingerprint};
pub use gradle::{parse_gradle_classpath_output, GradleBuild, GradleConfig};
pub use javac::{parse_javac_diagnostics, JavacDiagnosticFormat};
pub use maven::{parse_maven_classpath_output, MavenBuild, MavenConfig};

use nova_core::Diagnostic;
use std::path::{Path, PathBuf};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum BuildError {
    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("{tool} command failed with exit code {code:?}")]
    CommandFailed {
        tool: &'static str,
        code: Option<i32>,
        output: String,
    },

    #[error("failed to parse build output: {0}")]
    Parse(String),

    #[error(transparent)]
    Cache(#[from] cache::CacheError),

    #[error(transparent)]
    Project(#[from] nova_project::ProjectError),

    #[error("unsupported project layout: {0}")]
    Unsupported(String),
}

pub type Result<T> = std::result::Result<T, BuildError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum BuildSystemKind {
    Maven,
    Gradle,
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

/// Summary of a build invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildResult {
    pub diagnostics: Vec<Diagnostic>,
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
        let cache = BuildCache::new(cache_dir);
        Self {
            cache,
            maven: MavenBuild::new(MavenConfig::default()),
            gradle: GradleBuild::new(GradleConfig::default()),
        }
    }

    pub fn with_configs(
        cache_dir: impl Into<PathBuf>,
        maven: MavenConfig,
        gradle: GradleConfig,
    ) -> Self {
        let cache = BuildCache::new(cache_dir);
        Self {
            cache,
            maven: MavenBuild::new(maven),
            gradle: GradleBuild::new(gradle),
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
        self.maven.build(project_root, module_relative, &self.cache)
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
        self.gradle.build(project_root, project_path, &self.cache)
    }
}
