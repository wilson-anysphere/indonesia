use std::path::{Path, PathBuf};

use crate::{gradle, maven, simple, BuildSystem, ProjectConfig};

#[derive(Debug, Clone, Default)]
pub struct LoadOptions {
    /// Additional classpath entries (directories or jars) to include.
    ///
    /// This is primarily intended for Gradle projects where dependency resolution
    /// isn't implemented yet.
    pub classpath_overrides: Vec<PathBuf>,

    /// Override Maven local repository (`~/.m2/repository`) location.
    pub maven_repo: Option<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProjectError {
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse XML in {path}: {source}")]
    Xml {
        path: PathBuf,
        #[source]
        source: roxmltree::Error,
    },

    #[error("unsupported or empty workspace at {root}")]
    UnknownProjectType { root: PathBuf },
}

pub fn load_project(root: impl AsRef<Path>) -> Result<ProjectConfig, ProjectError> {
    load_project_with_options(root, &LoadOptions::default())
}

pub fn load_project_with_options(
    root: impl AsRef<Path>,
    options: &LoadOptions,
) -> Result<ProjectConfig, ProjectError> {
    let root = root.as_ref();
    let workspace_root = std::fs::canonicalize(root).map_err(|source| ProjectError::Io {
        path: root.to_path_buf(),
        source,
    })?;

    let build_system = detect_build_system(&workspace_root)?;

    match build_system {
        BuildSystem::Maven => maven::load_maven_project(&workspace_root, options),
        BuildSystem::Gradle => gradle::load_gradle_project(&workspace_root, options),
        BuildSystem::Simple => simple::load_simple_project(&workspace_root, options),
    }
}

pub fn reload_project(
    config: &ProjectConfig,
    _changed_files: &[PathBuf],
) -> Result<ProjectConfig, ProjectError> {
    // Naive implementation: re-scan the workspace root.
    load_project(&config.workspace_root)
}

fn detect_build_system(root: &Path) -> Result<BuildSystem, ProjectError> {
    if root.join("pom.xml").is_file() {
        return Ok(BuildSystem::Maven);
    }

    let gradle_markers = [
        "build.gradle",
        "build.gradle.kts",
        "settings.gradle",
        "settings.gradle.kts",
    ];
    if gradle_markers
        .iter()
        .any(|marker| root.join(marker).is_file())
    {
        return Ok(BuildSystem::Gradle);
    }

    // Fallback: "simple project" = any Java sources under `src/`.
    let has_java_sources = root.join("src").is_dir()
        && walkdir::WalkDir::new(root.join("src"))
            .into_iter()
            .filter_map(Result::ok)
            .any(|entry| entry.path().extension().is_some_and(|ext| ext == "java"));

    if has_java_sources {
        return Ok(BuildSystem::Simple);
    }

    Err(ProjectError::UnknownProjectType {
        root: root.to_path_buf(),
    })
}

