use std::path::{Path, PathBuf};

use nova_config::NovaConfig;

use crate::{bazel, gradle, maven, simple, BuildSystem, ProjectConfig, WorkspaceProjectModel};

#[derive(Debug, Clone, Default)]
pub struct LoadOptions {
    /// Additional classpath entries (directories or jars) to include.
    ///
    /// This is primarily intended for Gradle projects where dependency resolution
    /// is best-effort (Nova does not invoke Gradle).
    pub classpath_overrides: Vec<PathBuf>,

    /// Override Maven local repository (`~/.m2/repository`) location.
    pub maven_repo: Option<PathBuf>,

    /// Override Gradle user home (`~/.gradle`) location.
    ///
    /// When unset, Nova uses `GRADLE_USER_HOME` if present, otherwise falls back
    /// to `$HOME/.gradle` when `$HOME` is known.
    pub gradle_user_home: Option<PathBuf>,

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
    /// - `NOVA_BSP_PROGRAM`: BSP launcher executable (defaults to `bsp4bazel` when no `.bsp/*.json`
    ///   config is discovered)
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

    if reload_config {
        // Config changes can affect generated source roots, classpath overrides, etc.
        return load_project_from_workspace_root(workspace_root, options);
    }

    if changed_files.is_empty()
        || changed_files.iter().any(|path| {
            // If a build marker changes, the build system itself can change (e.g. a `pom.xml`
            // appears in a previously "simple" workspace). Treat *any* supported build file as a
            // signal to reload the full project configuration.
            is_build_file(BuildSystem::Maven, path)
                || is_build_file(BuildSystem::Gradle, path)
                || is_build_file(BuildSystem::Bazel, path)
                || is_apt_generated_roots_snapshot(path)
        })
    {
        // Build files changed (or unknown change set): rescan the workspace root.
        return load_project_from_workspace_root(workspace_root, options);
    }

    // `module-info.java` changes can toggle JPMS enablement for the entire workspace, which in
    // turn affects dependency classification (module-path vs classpath). Treat it like a build
    // file change and reload the full config to ensure `module_path`, `classpath`, and
    // `jpms_workspace` stay consistent.
    if changed_files.iter().any(|path| {
        path.file_name()
            .is_some_and(|name| name == "module-info.java")
    }) {
        return load_project_from_workspace_root(workspace_root, options);
    }

    // Source-only changes: keep the config stable.
    Ok(config.clone())
}

fn is_apt_generated_roots_snapshot(path: &Path) -> bool {
    // Nova uses `.nova/apt-cache/generated-roots.json` as a snapshot file for inferred generated
    // source roots from annotation processing. This file is read on project load, but may change
    // independently of build files or `nova.toml`, so treat it as a configuration-triggering file
    // for reloads.
    path.ends_with(
        Path::new(".nova")
            .join("apt-cache")
            .join("generated-roots.json"),
    )
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
    nova_build_model::bazel_workspace_root(start)
}

pub fn is_bazel_workspace(root: &Path) -> bool {
    nova_build_model::is_bazel_workspace(root)
}

/// Check whether a file change should trigger a full project reload for a given build system.
pub fn is_build_file(build_system: BuildSystem, path: &Path) -> bool {
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
                "maven-wrapper.jar" => path.ends_with(Path::new(".mvn/wrapper/maven-wrapper.jar")),
                _ => false,
            }
        }
        BuildSystem::Gradle => {
            // `.nova/queries/gradle.json` is a file-based handoff from `nova-build` to
            // `nova-project` that contains a Gradle snapshot (classpath, source roots, etc).
            //
            // Treat it as a build file so project reloads are triggered immediately when the
            // snapshot is updated.
            if path.ends_with(nova_build_model::GRADLE_SNAPSHOT_REL_PATH) {
                return true;
            }

            // Gradle project structure is extremely flexible:
            // - "script plugins" (`apply from: "deps.gradle"`) are often used to share config
            // - version catalogs (`*.versions.toml`) can change dependency resolution + plugins
            // - additional version catalogs can be configured via settings (e.g. `gradle/deps.versions.toml`)
            // - dependency locking (`gradle.lockfile`, `gradle/dependency-locks/*.lockfile`) can
            //   change resolved versions without touching build scripts
            // - wrapper changes affect which Gradle distribution is executed
            //
            // Treat these as "build files" so `reload_project()` re-loads configuration when they
            // change.
            let in_ignored_dir = path.components().any(|c| {
                c.as_os_str() == std::ffi::OsStr::new(".git")
                    || c.as_os_str() == std::ffi::OsStr::new(".gradle")
                    || c.as_os_str() == std::ffi::OsStr::new("build")
                    || c.as_os_str() == std::ffi::OsStr::new("target")
                    || c.as_os_str() == std::ffi::OsStr::new(".nova")
                    || c.as_os_str() == std::ffi::OsStr::new(".idea")
            });
            let is_gradle_version_catalog = !in_ignored_dir && name.ends_with(".versions.toml");
            let is_gradle_script_plugin =
                !in_ignored_dir && (name.ends_with(".gradle") || name.ends_with(".gradle.kts"));
            let is_gradle_dependency_lockfile = !in_ignored_dir
                && (name == "gradle.lockfile"
                    || (name.ends_with(".lockfile")
                        && path.parent().is_some_and(|parent| {
                            parent.ancestors().any(|dir| {
                                dir.file_name()
                                    .is_some_and(|name| name == "dependency-locks")
                            })
                        })));
            name == "gradle.properties"
                || is_gradle_version_catalog
                || is_gradle_dependency_lockfile
                || name == "gradlew"
                || name == "gradlew.bat"
                || name.starts_with("build.gradle")
                || name.starts_with("settings.gradle")
                || is_gradle_script_plugin
                || path.ends_with(Path::new("gradle/wrapper/gradle-wrapper.properties"))
                || path.ends_with(Path::new("gradle/wrapper/gradle-wrapper.jar"))
        }
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
                || (path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
                    && path
                        .parent()
                        .and_then(|parent| parent.file_name())
                        .is_some_and(|dir| dir == ".bsp"))
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
                || path.ends_with(Path::new("gradle/wrapper/gradle-wrapper.jar"))
                || path.extension().is_some_and(|ext| ext == "bzl")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ClasspathEntryKind;
    use nova_modules::ModuleName;
    use tempfile::tempdir;

    #[test]
    fn maven_build_file_detection_includes_mvn_wrapper_and_mvn_config() {
        let maven_build_markers = [
            "pom.xml",
            "mvnw",
            "mvnw.cmd",
            ".mvn/maven.config",
            ".mvn/jvm.config",
            ".mvn/extensions.xml",
            ".mvn/wrapper/maven-wrapper.jar",
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
    fn gradle_build_file_detection_includes_dependency_lockfiles() {
        let gradle_build_markers = [
            "gradle.lockfile",
            "gradle/dependency-locks/compileClasspath.lockfile",
        ];

        for path in gradle_build_markers {
            assert!(
                is_build_file(BuildSystem::Gradle, Path::new(path)),
                "expected {path} to be treated as a Gradle build marker"
            );
        }
    }

    #[test]
    fn bazel_workspace_root_picks_nearest_marker_and_matches_other_crates() {
        let tmp = tempdir().expect("tempdir");
        let outer = tmp.path();

        // Outer workspace marker.
        std::fs::write(outer.join("WORKSPACE"), "").expect("write outer WORKSPACE");

        // Nested workspace marker that should only be considered when starting inside it.
        let inner = outer.join("inner");
        std::fs::create_dir_all(inner.join("deep")).expect("mkdir inner/deep");
        std::fs::write(inner.join("WORKSPACE"), "").expect("write inner WORKSPACE");

        // A sibling subtree without its own marker; discovery should not walk *down* into `inner/`.
        let sibling = outer.join("sibling/deep");
        std::fs::create_dir_all(&sibling).expect("mkdir sibling/deep");

        // Starting inside `inner/` should resolve to the inner marker.
        let start_inner = inner.join("deep");
        let expected_inner = Some(inner.clone());
        assert_eq!(
            nova_build_model::bazel_workspace_root(&start_inner),
            expected_inner
        );
        assert_eq!(bazel_workspace_root(&start_inner), Some(inner.clone()));

        // Starting outside `inner/` should resolve to the outer marker, ignoring the deeper one.
        let expected_outer = Some(outer.to_path_buf());
        assert_eq!(
            nova_build_model::bazel_workspace_root(&sibling),
            expected_outer
        );
        assert_eq!(bazel_workspace_root(&sibling), Some(outer.to_path_buf()));

        #[cfg(feature = "bazel")]
        {
            assert_eq!(
                nova_build_bazel::bazel_workspace_root(&start_inner),
                Some(inner.clone())
            );
            assert_eq!(
                nova_build_bazel::bazel_workspace_root(&sibling),
                Some(outer.to_path_buf())
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

    #[test]
    fn maven_build_file_detection_is_path_aware_for_wrapper_jar() {
        assert!(
            !is_build_file(BuildSystem::Maven, Path::new("maven-wrapper.jar")),
            "misplaced maven-wrapper.jar at workspace root should not be treated as a build file"
        );
    }

    #[test]
    fn gradle_build_file_detection_includes_nova_gradle_snapshot() {
        assert!(
            is_build_file(
                BuildSystem::Gradle,
                Path::new(nova_build_model::GRADLE_SNAPSHOT_REL_PATH)
            ),
            ".nova/queries/gradle.json should be treated as a Gradle build marker"
        );
        assert!(
            !is_build_file(BuildSystem::Gradle, Path::new("gradle.json")),
            "only the .nova/queries/gradle.json snapshot should be treated as a build file"
        );
    }

    #[test]
    fn gradle_build_file_detection_includes_wrapper_jar() {
        assert!(
            is_build_file(
                BuildSystem::Gradle,
                Path::new("gradle/wrapper/gradle-wrapper.jar")
            ),
            "expected gradle/wrapper/gradle-wrapper.jar to be treated as a Gradle build marker"
        );
    }

    #[test]
    fn gradle_build_file_detection_is_path_aware_for_wrapper_jar() {
        assert!(
            !is_build_file(BuildSystem::Gradle, Path::new("gradle-wrapper.jar")),
            "misplaced gradle-wrapper.jar at workspace root should not be treated as a build file"
        );
    }

    #[test]
    fn simple_build_file_detection_includes_gradle_wrapper_jar() {
        assert!(
            is_build_file(
                BuildSystem::Simple,
                Path::new("gradle/wrapper/gradle-wrapper.jar")
            ),
            "expected gradle/wrapper/gradle-wrapper.jar to be treated as a Simple build marker so simple workspaces can reload/reclassify"
        );
        assert!(
            !is_build_file(BuildSystem::Simple, Path::new("gradle-wrapper.jar")),
            "misplaced gradle-wrapper.jar at workspace root should not be treated as a build file for Simple workspaces"
        );
    }

    #[test]
    fn reload_project_reloads_when_gradle_snapshot_changes() {
        let tmp = tempdir().expect("tempdir");
        let root = tmp.path();

        // Minimal Gradle workspace marker.
        std::fs::write(root.join("build.gradle"), "").expect("write build.gradle");

        let options = LoadOptions::default();
        let cfg = load_project_with_options(root, &options).expect("load project");
        assert_eq!(cfg.build_system, BuildSystem::Gradle);

        // Write a valid `.nova/queries/gradle.json` snapshot with a build fingerprint that matches
        // the workspace build files. This should influence Gradle project loading, so we can
        // observe `reload_project()` reloading configuration when the snapshot changes.
        let workspace_root = &cfg.workspace_root;
        let fingerprint = nova_build_model::collect_gradle_build_files(workspace_root)
            .and_then(|files| nova_build_model::BuildFileFingerprint::from_files(workspace_root, files))
            .expect("gradle build fingerprint")
            .digest;

        let snapshot_src = workspace_root.join("snapshot-src");
        std::fs::create_dir_all(&snapshot_src).expect("mkdir snapshot-src");
        assert!(
            !cfg.source_roots
                .iter()
                .any(|root| root.path == snapshot_src),
            "snapshot root should not be present before creating the gradle snapshot"
        );

        let snapshot_path = workspace_root.join(nova_build_model::GRADLE_SNAPSHOT_REL_PATH);
        std::fs::create_dir_all(snapshot_path.parent().expect("snapshot parent"))
            .expect("mkdir .nova/queries");

        let snapshot = serde_json::json!({
            "schemaVersion": nova_build_model::GRADLE_SNAPSHOT_SCHEMA_VERSION,
            "buildFingerprint": fingerprint,
            "projects": [{"path": ":", "projectDir": "."}],
            "javaCompileConfigs": {
                ":": {
                    "projectDir": ".",
                    "mainSourceRoots": ["snapshot-src"]
                }
            }
        });
        std::fs::write(
            &snapshot_path,
            serde_json::to_vec(&snapshot).expect("snapshot json"),
        )
        .expect("write gradle snapshot");

        let mut options_reload = options.clone();
        let cfg = reload_project(&cfg, &mut options_reload, &[snapshot_path.clone()])
            .expect("reload with gradle snapshot change");

        assert!(
            cfg.source_roots
                .iter()
                .any(|root| root.path == snapshot_src),
            "expected Gradle snapshot source root to be present after reload"
        );
    }

    #[test]
    fn reload_project_reclassifies_dependencies_when_module_info_changes() {
        let tmp = tempdir().expect("tempdir");
        let root = tmp.path();

        // Make this a simple workspace.
        std::fs::create_dir_all(root.join("src")).expect("mkdir src");
        std::fs::write(root.join("src/Main.java"), "class Main {}").expect("write Main.java");

        // Dependency override entry (directory) containing `module-info.class`.
        let dep_dir = root.join("deps/mod-b");
        std::fs::create_dir_all(&dep_dir).expect("mkdir dep");
        std::fs::write(dep_dir.join("module-info.class"), make_module_info_class())
            .expect("write module-info.class");

        let mut options = LoadOptions::default();
        options.classpath_overrides.push(dep_dir.clone());

        // No `module-info.java` => JPMS disabled => dependency stays on classpath.
        let cfg = load_project_with_options(root, &options).expect("load project");
        assert_eq!(cfg.build_system, BuildSystem::Simple);
        assert!(
            cfg.jpms_modules.is_empty(),
            "without module-info.java, workspace should not use JPMS"
        );
        assert!(cfg.jpms_workspace.is_none());
        assert!(cfg.module_path.is_empty());
        assert!(
            cfg.classpath
                .iter()
                .any(|e| e.path == dep_dir && e.kind == ClasspathEntryKind::Directory),
            "dependency should be on classpath when JPMS is disabled"
        );

        // Add `module-info.java` to enable JPMS and trigger reload.
        let module_info_path = root.join("module-info.java");
        std::fs::write(&module_info_path, "module mod.a { requires mod.b; }")
            .expect("write module-info.java");

        let mut options_reload = options.clone();
        let cfg = reload_project(&cfg, &mut options_reload, &[module_info_path.clone()])
            .expect("reload with module-info.java added");

        assert!(
            !cfg.jpms_modules.is_empty(),
            "adding module-info.java should enable JPMS"
        );
        assert!(cfg.jpms_workspace.is_some());
        assert!(
            cfg.module_path
                .iter()
                .any(|e| e.path == dep_dir && e.kind == ClasspathEntryKind::Directory),
            "dependency should move to module-path when JPMS is enabled"
        );
        assert!(
            !cfg.classpath
                .iter()
                .any(|e| e.path == dep_dir && e.kind == ClasspathEntryKind::Directory),
            "dependency should no longer be on classpath when JPMS is enabled"
        );

        let graph = cfg.module_graph().expect("module graph");
        let mod_a = ModuleName::new("mod.a");
        let mod_b = ModuleName::new("mod.b");
        assert!(
            graph.get(&mod_a).is_some(),
            "graph should contain workspace module"
        );
        assert!(
            graph.get(&mod_b).is_some(),
            "graph should contain dependency module from module-path"
        );

        // Remove `module-info.java` to disable JPMS and reload again.
        std::fs::remove_file(&module_info_path).expect("delete module-info.java");
        let cfg = reload_project(&cfg, &mut options_reload, &[module_info_path.clone()])
            .expect("reload with module-info.java removed");

        assert!(
            cfg.jpms_modules.is_empty(),
            "removing module-info.java should disable JPMS"
        );
        assert!(cfg.jpms_workspace.is_none());
        assert!(cfg.module_path.is_empty(), "module-path should be cleared");
        assert!(
            cfg.classpath
                .iter()
                .any(|e| e.path == dep_dir && e.kind == ClasspathEntryKind::Directory),
            "dependency should return to classpath when JPMS is disabled"
        );
    }

    fn make_module_info_class() -> Vec<u8> {
        fn push_u2(out: &mut Vec<u8>, v: u16) {
            out.extend_from_slice(&v.to_be_bytes());
        }
        fn push_u4(out: &mut Vec<u8>, v: u32) {
            out.extend_from_slice(&v.to_be_bytes());
        }

        #[derive(Clone)]
        enum CpEntry {
            Utf8(String),
            Module { name_index: u16 },
            Package { name_index: u16 },
        }

        struct Cp {
            entries: Vec<CpEntry>,
        }

        impl Cp {
            fn new() -> Self {
                Self {
                    entries: Vec::new(),
                }
            }

            fn push(&mut self, entry: CpEntry) -> u16 {
                self.entries.push(entry);
                self.entries.len() as u16
            }

            fn utf8(&mut self, s: &str) -> u16 {
                self.push(CpEntry::Utf8(s.to_string()))
            }

            fn module(&mut self, name: &str) -> u16 {
                let name_index = self.utf8(name);
                self.push(CpEntry::Module { name_index })
            }

            fn package(&mut self, name: &str) -> u16 {
                let name_index = self.utf8(name);
                self.push(CpEntry::Package { name_index })
            }

            fn write(&self, out: &mut Vec<u8>) {
                push_u2(out, (self.entries.len() as u16) + 1);
                for entry in &self.entries {
                    match entry {
                        CpEntry::Utf8(s) => {
                            out.push(1);
                            push_u2(out, s.len() as u16);
                            out.extend_from_slice(s.as_bytes());
                        }
                        CpEntry::Module { name_index } => {
                            out.push(19);
                            push_u2(out, *name_index);
                        }
                        CpEntry::Package { name_index } => {
                            out.push(20);
                            push_u2(out, *name_index);
                        }
                    }
                }
            }
        }

        let mut cp = Cp::new();
        let module_attr_name = cp.utf8("Module");
        let self_module = cp.module("mod.b");
        let api_pkg = cp.package("com/example/b/api");
        let _internal_pkg = cp.package("com/example/b/internal");
        let target_mod = cp.module("mod.a");

        let mut module_attr = Vec::new();
        push_u2(&mut module_attr, self_module); // module_name_index
        push_u2(&mut module_attr, 0); // module_flags
        push_u2(&mut module_attr, 0); // module_version_index
        push_u2(&mut module_attr, 0); // requires_count
        push_u2(&mut module_attr, 1); // exports_count
                                      // exports
        push_u2(&mut module_attr, api_pkg); // exports_index (Package)
        push_u2(&mut module_attr, 0); // exports_flags
        push_u2(&mut module_attr, 1); // exports_to_count
        push_u2(&mut module_attr, target_mod); // exports_to_index (Module)
        push_u2(&mut module_attr, 0); // opens_count
        push_u2(&mut module_attr, 0); // uses_count
        push_u2(&mut module_attr, 0); // provides_count

        let mut out = Vec::new();
        push_u4(&mut out, 0xCAFEBABE);
        push_u2(&mut out, 0); // minor
        push_u2(&mut out, 53); // major (Java 9)
        cp.write(&mut out);

        push_u2(&mut out, 0); // access_flags
        push_u2(&mut out, 0); // this_class
        push_u2(&mut out, 0); // super_class
        push_u2(&mut out, 0); // interfaces_count
        push_u2(&mut out, 0); // fields_count
        push_u2(&mut out, 0); // methods_count

        push_u2(&mut out, 1); // attributes_count
        push_u2(&mut out, module_attr_name);
        push_u4(&mut out, module_attr.len() as u32);
        out.extend_from_slice(&module_attr);

        // Sanity check: ensure the fixture parses.
        let info = nova_classfile::parse_module_info_class(&out).expect("parse module-info.class");
        assert_eq!(info.name.as_str(), "mod.b");
        assert!(info.exports_package_to("com.example.b.api", &ModuleName::new("mod.a")));

        out
    }
}
