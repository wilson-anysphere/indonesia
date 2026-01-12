use std::path::{Path, PathBuf};

use nova_config::NovaConfig;

use crate::{bazel, gradle, maven, simple, BuildSystem, ProjectConfig, WorkspaceProjectModel};

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
    ///
    /// ### Optional BSP support
    ///
    /// `nova-build-bazel` can optionally use Bazel's Build Server Protocol (BSP) for
    /// target discovery and compile info. Downstream users can enable this end-to-end
    /// by compiling `nova-project` with the `bazel-bsp` feature:
    ///
    /// - `cargo ... --features nova-project/bazel-bsp` (workspace feature syntax)
    ///
    /// Runtime knobs (read by `nova-build-bazel` when BSP support is compiled in):
    /// - `NOVA_BAZEL_USE_BSP`: set to `0`/`false` to force `bazel query`/`aquery`
    /// - `NOVA_BSP_PROGRAM`: BSP launcher executable (defaults to `bsp4bazel`)
    /// - `NOVA_BSP_ARGS`: launcher args (JSON array or whitespace-separated string)
    ///
    /// Nova also supports standard BSP `.bsp/*.json` config discovery (when BSP support is
    /// compiled in). Environment variables still override any discovered config.
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

    /// Optional Bazel query universe expression used for Java target discovery.
    ///
    /// When set, Nova will scope target discovery to the provided expression, replacing the
    /// default `//...` universe. This can dramatically improve startup performance in large
    /// monorepos.
    ///
    /// Example: `deps(//my/app:app)`.
    pub target_universe: Option<String>,

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
            target_universe: None,
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

pub fn load_workspace_model(root: impl AsRef<Path>) -> Result<WorkspaceProjectModel, ProjectError> {
    load_workspace_model_with_workspace_config(root)
}

pub fn load_project_with_workspace_config(
    root: impl AsRef<Path>,
) -> Result<ProjectConfig, ProjectError> {
    let start_path = crate::workspace_config::canonicalize_workspace_root(root)?;
    let workspace_root =
        workspace_root(&start_path).ok_or(ProjectError::UnknownProjectType { root: start_path })?;
    let (nova_config, nova_config_path) =
        crate::workspace_config::load_nova_config(&workspace_root)?;
    let options = LoadOptions {
        nova_config,
        nova_config_path,
        ..LoadOptions::default()
    };

    load_project_from_workspace_root(&workspace_root, &options)
}

pub fn load_workspace_model_with_workspace_config(
    root: impl AsRef<Path>,
) -> Result<WorkspaceProjectModel, ProjectError> {
    let start_path = crate::workspace_config::canonicalize_workspace_root(root)?;
    let workspace_root =
        workspace_root(&start_path).ok_or(ProjectError::UnknownProjectType { root: start_path })?;
    let (nova_config, nova_config_path) =
        crate::workspace_config::load_nova_config(&workspace_root)?;
    let options = LoadOptions {
        nova_config,
        nova_config_path,
        ..LoadOptions::default()
    };

    load_workspace_model_from_workspace_root(&workspace_root, &options)
}

pub fn load_project_with_options(
    root: impl AsRef<Path>,
    options: &LoadOptions,
) -> Result<ProjectConfig, ProjectError> {
    let start_path = crate::workspace_config::canonicalize_workspace_root(root)?;
    let workspace_root =
        workspace_root(&start_path).ok_or(ProjectError::UnknownProjectType { root: start_path })?;
    load_project_from_workspace_root(&workspace_root, options)
}

pub fn load_workspace_model_with_options(
    root: impl AsRef<Path>,
    options: &LoadOptions,
) -> Result<WorkspaceProjectModel, ProjectError> {
    let start_path = crate::workspace_config::canonicalize_workspace_root(root)?;
    let workspace_root =
        workspace_root(&start_path).ok_or(ProjectError::UnknownProjectType { root: start_path })?;
    load_workspace_model_from_workspace_root(&workspace_root, options)
}

pub fn reload_project(
    config: &ProjectConfig,
    options: &mut LoadOptions,
    changed_files: &[PathBuf],
) -> Result<ProjectConfig, ProjectError> {
    let workspace_root = &config.workspace_root;

    let discovered_path = nova_config::discover_config_path(workspace_root);
    let reload_config = changed_files.iter().any(|changed| {
        changed.ends_with(Path::new(".nova/apt-cache/generated-roots.json"))
            || options
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

    if reload_config {
        // Config changes can affect generated source roots, classpath overrides, etc.
        return load_project_from_workspace_root(workspace_root, options);
    }

    if changed_files.is_empty()
        || changed_files
            .iter()
            .any(|path| is_build_file(config.build_system, path))
    {
        // Build files changed (or unknown change set): rescan the workspace root.
        return load_project_from_workspace_root(workspace_root, options);
    }

    // Module-info changes affect the JPMS module root list + workspace graph; update it without
    // re-loading the module list.
    if changed_files.iter().any(|path| {
        path.file_name()
            .is_some_and(|name| name == "module-info.java")
    }) {
        let mut next = config.clone();
        next.jpms_modules = crate::jpms::discover_jpms_modules(&next.modules);
        next.jpms_workspace =
            crate::jpms::build_jpms_workspace(&next.jpms_modules, &next.module_path);
        return Ok(next);
    }

    // Source-only changes: keep the config stable.
    Ok(config.clone())
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

fn load_workspace_model_from_workspace_root(
    workspace_root: &Path,
    options: &LoadOptions,
) -> Result<WorkspaceProjectModel, ProjectError> {
    let build_system = detect_build_system(workspace_root)?;

    match build_system {
        BuildSystem::Maven => maven::load_maven_workspace_model(workspace_root, options),
        BuildSystem::Gradle => gradle::load_gradle_workspace_model(workspace_root, options),
        BuildSystem::Bazel => bazel::load_bazel_workspace_model(workspace_root, options),
        BuildSystem::Simple => simple::load_simple_workspace_model(workspace_root, options),
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

    // Fallback: "simple project" = `src/` folder exists.
    if root.join("src").is_dir() {
        return Ok(BuildSystem::Simple);
    }

    Err(ProjectError::UnknownProjectType {
        root: root.to_path_buf(),
    })
}

/// Discover the workspace root for a given path.
///
/// `start` may be either a directory or a file path within a workspace.
/// The search is deterministic and stops at the filesystem root.
///
/// Build system heuristics:
/// - Bazel: the nearest ancestor with a `WORKSPACE`, `WORKSPACE.bazel`, or `MODULE.bazel` file.
/// - Maven: walk upward looking for `pom.xml` and prefer the outermost aggregator pom (pom with
///   `<modules>`). If no aggregator is found, fall back to the nearest `pom.xml`.
/// - Gradle: walk upward looking for `settings.gradle(.kts)`; if not found, fall back to the
///   nearest directory containing `build.gradle(.kts)`.
/// - Simple: if no build system markers are found, fall back to the nearest directory containing
///   `src/`.
pub fn workspace_root(start: impl AsRef<Path>) -> Option<PathBuf> {
    let start = start.as_ref();
    let start_dir = if start.is_file() {
        start.parent()?
    } else {
        start
    };

    // If the caller explicitly provided a directory that already looks like a self-contained
    // "simple project" root, prefer it over ancestor build markers. This prevents unrelated
    // files in shared temp directories (e.g. `/tmp`) from "stealing" workspace root discovery.
    if start_dir.join("src").is_dir()
        && !start_dir.join("pom.xml").is_file()
        && !has_gradle_settings(start_dir)
        && !has_gradle_build(start_dir)
        && !is_bazel_workspace(start_dir)
    {
        return Some(start_dir.to_path_buf());
    }

    let bazel_root = bazel_workspace_root(start_dir);
    let maven_root = maven_workspace_root(start_dir);
    let gradle_root = gradle_workspace_root(start_dir);

    // Prefer "real" build systems (Bazel/Maven/Gradle). `Simple` is a last resort, because many
    // non-Java projects also contain a `src/` directory.
    pick_best_workspace_root(&[
        (BuildSystem::Bazel, bazel_root),
        (BuildSystem::Maven, maven_root),
        (BuildSystem::Gradle, gradle_root),
    ])
    .or_else(|| simple_workspace_root(start_dir))
}

fn pick_best_workspace_root(candidates: &[(BuildSystem, Option<PathBuf>)]) -> Option<PathBuf> {
    fn priority(system: BuildSystem) -> u8 {
        match system {
            BuildSystem::Bazel => 0,
            BuildSystem::Maven => 1,
            BuildSystem::Gradle => 2,
            BuildSystem::Simple => 3,
        }
    }

    candidates
        .iter()
        .filter_map(|(system, root)| root.as_ref().map(|root| (*system, root)))
        .max_by(|(a_sys, a_root), (b_sys, b_root)| {
            a_root
                .components()
                .count()
                .cmp(&b_root.components().count())
                // If the root is the same depth (likely the same directory), pick a stable
                // precedence order.
                .then_with(|| priority(*b_sys).cmp(&priority(*a_sys)))
        })
        .map(|(_, root)| root.to_path_buf())
}

fn maven_workspace_root(start: &Path) -> Option<PathBuf> {
    let mut dir = start;
    let mut nearest_pom = None;
    let mut outermost_aggregator = None;

    loop {
        let pom = dir.join("pom.xml");
        if pom.is_file() {
            if nearest_pom.is_none() {
                nearest_pom = Some(dir.to_path_buf());
            }
            if pom_has_modules(&pom) {
                outermost_aggregator = Some(dir.to_path_buf());
            }
        }

        let Some(parent) = dir.parent() else {
            break;
        };
        if parent == dir {
            break;
        }
        dir = parent;
    }

    outermost_aggregator.or(nearest_pom)
}

fn pom_has_modules(pom_path: &Path) -> bool {
    let Ok(contents) = std::fs::read_to_string(pom_path) else {
        return false;
    };
    let Ok(doc) = roxmltree::Document::parse(&contents) else {
        return false;
    };

    doc.root()
        .descendants()
        .any(|node| node.is_element() && node.tag_name().name() == "modules")
}

fn gradle_workspace_root(start: &Path) -> Option<PathBuf> {
    let mut dir = start;
    let mut nearest_build = None;

    loop {
        if has_gradle_settings(dir) {
            return Some(dir.to_path_buf());
        }

        if nearest_build.is_none() && has_gradle_build(dir) {
            nearest_build = Some(dir.to_path_buf());
        }

        let Some(parent) = dir.parent() else {
            break;
        };
        if parent == dir {
            break;
        }
        dir = parent;
    }

    nearest_build
}

fn has_gradle_settings(dir: &Path) -> bool {
    dir.join("settings.gradle").is_file() || dir.join("settings.gradle.kts").is_file()
}

fn has_gradle_build(dir: &Path) -> bool {
    dir.join("build.gradle").is_file() || dir.join("build.gradle.kts").is_file()
}

fn simple_workspace_root(start: &Path) -> Option<PathBuf> {
    let mut dir = start;
    loop {
        if dir.join("src").is_dir() {
            return Some(dir.to_path_buf());
        }

        let Some(parent) = dir.parent() else {
            break;
        };
        if parent == dir {
            break;
        }
        dir = parent;
    }
    None
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

fn is_build_file(build_system: BuildSystem, path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };

    match build_system {
        BuildSystem::Maven => {
            if matches!(name, "pom.xml" | "mvnw" | "mvnw.cmd") {
                return true;
            }

            match name {
                "maven.config" => path.ends_with(Path::new(".mvn/maven.config")),
                "jvm.config" => path.ends_with(Path::new(".mvn/jvm.config")),
                "extensions.xml" => path.ends_with(Path::new(".mvn/extensions.xml")),
                "maven-wrapper.properties" => {
                    path.ends_with(Path::new(".mvn/wrapper/maven-wrapper.properties"))
                }
                _ => false,
            }
        }
        BuildSystem::Gradle => matches!(
            name,
            "build.gradle"
                | "build.gradle.kts"
                | "settings.gradle"
                | "settings.gradle.kts"
                | "gradle.properties"
        ),
        BuildSystem::Bazel => {
            matches!(
                name,
                "WORKSPACE"
                    | "WORKSPACE.bazel"
                    | "MODULE.bazel"
                    | "MODULE.bazel.lock"
                    | "BUILD"
                    | "BUILD.bazel"
                    | ".bazelrc"
                    | ".bazelversion"
                    | "bazelisk.rc"
                    | ".bazelignore"
            ) || name.starts_with(".bazelrc.")
                || path.extension().is_some_and(|ext| ext == "bzl")
        }
        BuildSystem::Simple => {
            // Simple projects can "upgrade" to another build system as soon as a marker file
            // appears (e.g. creating a new `pom.xml`). Treat all supported build files as reload
            // triggers so callers can re-detect the workspace model.
            name == "pom.xml"
                || matches!(
                    name,
                    "build.gradle"
                        | "build.gradle.kts"
                        | "settings.gradle"
                        | "settings.gradle.kts"
                        | "gradle.properties"
                        | "WORKSPACE"
                        | "WORKSPACE.bazel"
                        | "MODULE.bazel"
                        | "BUILD"
                        | "BUILD.bazel"
                )
                || path.extension().is_some_and(|ext| ext == "bzl")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maven_build_file_detection_includes_mvn_wrapper_and_mvn_config() {
        let maven_build_markers = [
            "pom.xml",
            "mvnw",
            "mvnw.cmd",
            ".mvn/maven.config",
            ".mvn/jvm.config",
            ".mvn/extensions.xml",
            ".mvn/wrapper/maven-wrapper.properties",
        ];

        for path in maven_build_markers {
            assert!(
                is_build_file(BuildSystem::Maven, Path::new(path)),
                "expected {path} to be treated as a Maven build marker"
            );
        }
    }

    #[test]
    fn maven_build_file_detection_is_path_aware_for_wrapper_properties() {
        assert!(
            !is_build_file(BuildSystem::Maven, Path::new("maven-wrapper.properties")),
            "misplaced maven-wrapper.properties at workspace root should not be treated as a build file"
        );
    }
}
