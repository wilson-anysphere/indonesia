use std::path::{Path, PathBuf};

use nova_config::NovaConfig;

use crate::{bazel, gradle, maven, simple, BuildSystem, ProjectConfig};

#[derive(Debug, Clone, Default)]
pub struct LoadOptions {
    /// Additional classpath entries (directories or jars) to include.
    ///
    /// This is primarily intended for Gradle projects where dependency resolution
    /// isn't implemented yet.
    pub classpath_overrides: Vec<PathBuf>,

    /// Override Maven local repository (`~/.m2/repository`) location.
    pub maven_repo: Option<PathBuf>,

    /// Nova-specific configuration (e.g. generated source roots).
    pub nova_config: NovaConfig,

    /// Path to the on-disk config used to populate `nova_config`, if any.
    ///
    /// This is tracked so callers can cheaply determine whether a config reload
    /// is needed when a file watcher reports changes.
    pub nova_config_path: Option<PathBuf>,

    /// Bazel-specific loader configuration.
    ///
    /// By default Nova uses a heuristic (treat BUILD directories as source roots) to
    /// avoid invoking Bazel unexpectedly. Enable `bazel.enable_target_loading` to
    /// populate per-target compilation metadata by running `bazel query`/`aquery`.
    pub bazel: BazelLoadOptions,
}

#[derive(Debug, Clone)]
pub struct BazelLoadOptions {
    /// When enabled, Nova invokes Bazel to build a target-aware project model.
    ///
    /// This runs:
    /// - `bazel query kind("java_.* rule", //...)` to discover Java targets
    /// - `bazel aquery` per target to extract `javac` settings
    pub enable_target_loading: bool,

    /// Cap the number of targets for which we will execute `aquery`.
    ///
    /// Large workspaces can have thousands of targets; this avoids loading too much
    /// data on startup. Targets are sorted lexicographically before applying the
    /// limit for determinism.
    pub target_limit: usize,

    /// Optional explicit target list to load.
    ///
    /// When set, only these targets are loaded (and `target_limit` is applied).
    pub targets: Option<Vec<String>>,
}

impl Default for BazelLoadOptions {
    fn default() -> Self {
        Self {
            enable_target_loading: false,
            target_limit: 200,
            targets: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProjectError {
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(transparent)]
    Config(#[from] nova_config::ConfigError),

    #[error("failed to parse XML in {path}: {source}")]
    Xml {
        path: PathBuf,
        #[source]
        source: roxmltree::Error,
    },

    #[error("unsupported or empty workspace at {root}")]
    UnknownProjectType { root: PathBuf },

    #[error("bazel integration failed: {message}")]
    Bazel { message: String },
}

pub fn load_project(root: impl AsRef<Path>) -> Result<ProjectConfig, ProjectError> {
    load_project_with_workspace_config(root)
}

pub fn load_project_with_workspace_config(
    root: impl AsRef<Path>,
) -> Result<ProjectConfig, ProjectError> {
    let workspace_root = crate::workspace_config::canonicalize_workspace_root(root)?;
    let (nova_config, nova_config_path) =
        crate::workspace_config::load_nova_config(&workspace_root)?;
    let options = LoadOptions {
        nova_config,
        nova_config_path,
        ..LoadOptions::default()
    };

    load_project_from_workspace_root(&workspace_root, &options)
}

pub fn load_project_with_options(
    root: impl AsRef<Path>,
    options: &LoadOptions,
) -> Result<ProjectConfig, ProjectError> {
    let workspace_root = crate::workspace_config::canonicalize_workspace_root(root)?;
    load_project_from_workspace_root(&workspace_root, options)
}

pub fn reload_project(
    config: &ProjectConfig,
    options: &mut LoadOptions,
    changed_files: &[PathBuf],
) -> Result<ProjectConfig, ProjectError> {
    let workspace_root = &config.workspace_root;

    let discovered_path = nova_config::discover_config_path(workspace_root);
    let reload_config = changed_files.iter().any(|changed| {
        options
            .nova_config_path
            .as_ref()
            .is_some_and(|p| p == changed)
            || discovered_path.as_ref().is_some_and(|p| p == changed)
    });

    if reload_config {
        let (new_config, new_path, _changed) = nova_config::reload_for_workspace(
            workspace_root,
            &options.nova_config,
            options.nova_config_path.as_deref(),
        )?;
        options.nova_config = new_config;
        options.nova_config_path = new_path;
    }

    // Naive implementation: re-scan the workspace root.
    load_project_from_workspace_root(workspace_root, options)
}

fn load_project_from_workspace_root(
    workspace_root: &Path,
    options: &LoadOptions,
) -> Result<ProjectConfig, ProjectError> {
    let build_system = detect_build_system(workspace_root)?;

    match build_system {
        BuildSystem::Maven => maven::load_maven_project(workspace_root, options),
        BuildSystem::Gradle => gradle::load_gradle_project(workspace_root, options),
        BuildSystem::Bazel => bazel::load_bazel_project(workspace_root, options),
        BuildSystem::Simple => simple::load_simple_project(workspace_root, options),
    }
}

fn detect_build_system(root: &Path) -> Result<BuildSystem, ProjectError> {
    if is_bazel_workspace(root) {
        return Ok(BuildSystem::Bazel);
    }

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

/// Walk upwards from `start` to find the Bazel workspace root.
///
/// A workspace root is identified by the presence of one of:
/// - `WORKSPACE`
/// - `WORKSPACE.bazel`
/// - `MODULE.bazel`
pub fn bazel_workspace_root(start: impl AsRef<Path>) -> Option<PathBuf> {
    let start = start.as_ref();
    let mut dir = if start.is_file() {
        start.parent()?
    } else {
        start
    };

    loop {
        if is_bazel_workspace(dir) {
            return Some(dir.to_path_buf());
        }
        dir = dir.parent()?;
    }
}

pub fn is_bazel_workspace(root: &Path) -> bool {
    ["WORKSPACE", "WORKSPACE.bazel", "MODULE.bazel"]
        .iter()
        .any(|marker| root.join(marker).is_file())
}
