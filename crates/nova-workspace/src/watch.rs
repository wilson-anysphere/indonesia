use std::path::{Path, PathBuf};

fn normalize_watch_path(path: impl Into<PathBuf>) -> PathBuf {
    match nova_vfs::VfsPath::local(path.into()) {
        nova_vfs::VfsPath::Local(path) => path,
        // `VfsPath::local` always returns the local variant.
        _ => unreachable!("VfsPath::local produced a non-local path"),
    }
}

#[derive(Debug, Clone, Copy, Ord, PartialOrd, Eq, PartialEq)]
pub enum ChangeCategory {
    Source,
    Build,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NormalizedEvent {
    Created(PathBuf),
    Modified(PathBuf),
    Deleted(PathBuf),
    Moved { from: PathBuf, to: PathBuf },
}

impl NormalizedEvent {
    pub fn paths(&self) -> impl Iterator<Item = &Path> {
        let (first, second) = match self {
            NormalizedEvent::Created(p)
            | NormalizedEvent::Modified(p)
            | NormalizedEvent::Deleted(p) => (p.as_path(), None),
            NormalizedEvent::Moved { from, to } => (from.as_path(), Some(to.as_path())),
        };

        std::iter::once(first).chain(second.into_iter())
    }
}

#[derive(Debug, Clone)]
pub struct WatchConfig {
    /// Workspace root (used to classify `.java` files).
    pub workspace_root: PathBuf,
    pub source_roots: Vec<PathBuf>,
    pub generated_source_roots: Vec<PathBuf>,
    /// Path to the Nova config file used for this workspace (if any).
    ///
    /// This may point outside of `workspace_root` when `NOVA_CONFIG_PATH` is set.
    pub nova_config_path: Option<PathBuf>,
}

impl WatchConfig {
    pub fn new(workspace_root: PathBuf) -> Self {
        Self::with_roots(workspace_root, Vec::new(), Vec::new())
    }

    pub fn with_roots(
        workspace_root: PathBuf,
        source_roots: Vec<PathBuf>,
        generated_source_roots: Vec<PathBuf>,
    ) -> Self {
        Self {
            workspace_root: normalize_watch_path(workspace_root),
            source_roots: source_roots
                .into_iter()
                .map(|root| normalize_watch_path(root))
                .collect(),
            generated_source_roots: generated_source_roots
                .into_iter()
                .map(|root| normalize_watch_path(root))
                .collect(),
            nova_config_path: None,
        }
    }
}

pub fn categorize_event(config: &WatchConfig, event: &NormalizedEvent) -> Option<ChangeCategory> {
    for path in event.paths() {
        if config
            .nova_config_path
            .as_ref()
            .is_some_and(|config_path| config_path == path)
        {
            return Some(ChangeCategory::Build);
        }
        // `module-info.java` updates the JPMS module graph embedded in `ProjectConfig`. Treat it
        // like a build change so we reload the project config instead of only updating file
        // contents.
        if path
            .file_name()
            .is_some_and(|name| name == "module-info.java")
        {
            return Some(ChangeCategory::Build);
        }
        // Many build files are detected based on path components (e.g. ignoring `build/` output
        // directories). Use paths relative to the workspace root so absolute parent directories
        // (like `/home/user/build/...`) don't accidentally trip ignore heuristics.
        let rel = path.strip_prefix(&config.workspace_root).unwrap_or(path);
        if is_build_file(rel) {
            return Some(ChangeCategory::Build);
        }
    }

    // We only index Java sources.
    for path in event.paths() {
        if path.extension().and_then(|s| s.to_str()) != Some("java") {
            continue;
        }
        let has_configured_roots =
            !config.source_roots.is_empty() || !config.generated_source_roots.is_empty();

        if has_configured_roots {
            if is_within_any(path, &config.source_roots)
                || is_within_any(path, &config.generated_source_roots)
            {
                return Some(ChangeCategory::Source);
            }
        } else if path.starts_with(&config.workspace_root) {
            // Fall back to treating the entire workspace root as a source root when we don't have
            // more specific roots (e.g. simple projects).
            return Some(ChangeCategory::Source);
        }
    }

    None
}

fn is_within_any(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| path.starts_with(root))
}

pub fn is_build_file(path: &Path) -> bool {
    if path.ends_with(Path::new(".nova/queries/gradle.json")) {
        return true;
    }

    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return false;
    };

    // Mirror `nova-build` Gradle build-file fingerprinting exclusions to avoid
    // treating build output / cache directories as project-changing inputs.
    let in_ignored_dir = path.components().any(|c| {
        c.as_os_str() == std::ffi::OsStr::new(".git")
            || c.as_os_str() == std::ffi::OsStr::new(".gradle")
            || c.as_os_str() == std::ffi::OsStr::new("build")
            || c.as_os_str() == std::ffi::OsStr::new("target")
            || c.as_os_str() == std::ffi::OsStr::new(".nova")
            || c.as_os_str() == std::ffi::OsStr::new(".idea")
    });

    // Nova's generated-source roots discovery reads a snapshot under
    // `.nova/apt-cache/generated-roots.json`. Updates to this file should
    // trigger a project reload so newly discovered generated roots are watched.
    if name == "generated-roots.json"
        && path.ends_with(Path::new(".nova/apt-cache/generated-roots.json"))
    {
        return true;
    }

    // Gradle script plugins can influence dependencies and tasks.
    if !in_ignored_dir && (name.ends_with(".gradle") || name.ends_with(".gradle.kts")) {
        return true;
    }

    // Gradle version catalogs can define dependency versions.
    if !in_ignored_dir && name == "libs.versions.toml" {
        return true;
    }

    // Gradle version catalogs can also be custom-named, but they must live directly under a
    // `gradle/` directory (e.g. `gradle/foo.versions.toml`).
    if !in_ignored_dir
        && name.ends_with(".versions.toml")
        && path
            .parent()
            .and_then(|p| p.file_name())
            .is_some_and(|dir| dir == "gradle")
    {
        return true;
    }

    if name == "pom.xml"
        || name == "module-info.java"
        || name == "nova.toml"
        || name == ".nova.toml"
        || name == "nova.config.toml"
        || name == ".bazelrc"
        || name.starts_with(".bazelrc.")
        || name == ".bazelversion"
        || name == "MODULE.bazel.lock"
        || name == "bazelisk.rc"
        || name == ".bazelignore"
        || name.starts_with("build.gradle")
        || name.starts_with("settings.gradle")
        || matches!(
            name,
            "BUILD" | "BUILD.bazel" | "WORKSPACE" | "WORKSPACE.bazel" | "MODULE.bazel"
        )
    {
        return true;
    }

    if name == "config.toml" && path.ends_with(Path::new(".nova/config.toml")) {
        return true;
    }

    if path.extension().and_then(|s| s.to_str()) == Some("bzl") {
        return true;
    }

    match name {
        "gradle.properties" | "gradlew" | "gradlew.bat" => true,
        "gradle-wrapper.properties" => {
            path.ends_with(Path::new("gradle/wrapper/gradle-wrapper.properties"))
        }
        "gradle-wrapper.jar" => path.ends_with(Path::new("gradle/wrapper/gradle-wrapper.jar")),
        "mvnw" | "mvnw.cmd" => true,
        "maven-wrapper.properties" => {
            path.ends_with(Path::new(".mvn/wrapper/maven-wrapper.properties"))
        }
        "maven-wrapper.jar" => path.ends_with(Path::new(".mvn/wrapper/maven-wrapper.jar")),
        "extensions.xml" => path.ends_with(Path::new(".mvn/extensions.xml")),
        "maven.config" => path.ends_with(Path::new(".mvn/maven.config")),
        "jvm.config" => path.ends_with(Path::new(".mvn/jvm.config")),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_file_changes_are_categorized_as_build() {
        let root = PathBuf::from("/tmp/workspace");
        let config = WatchConfig::new(root.clone());

        let build_files = [
            root.join("pom.xml"),
            root.join("module-info.java"),
            root.join("nova.toml"),
            root.join(".nova.toml"),
            root.join("nova.config.toml"),
            root.join(".nova").join("config.toml"),
            root.join(".nova").join("queries").join("gradle.json"),
            root.join(".bazelrc"),
            root.join(".bazelrc.user"),
            root.join(".bazelversion"),
            root.join("MODULE.bazel.lock"),
            root.join("bazelisk.rc"),
            root.join(".bazelignore"),
            root.join("build.gradle"),
            root.join("build.gradle.kts"),
            root.join("settings.gradle"),
            root.join("settings.gradle.kts"),
            root.join("gradle.properties"),
            root.join("gradlew"),
            root.join("gradlew.bat"),
            root.join("gradle")
                .join("wrapper")
                .join("gradle-wrapper.properties"),
            root.join("gradle").join("wrapper").join("gradle-wrapper.jar"),
            root.join("mvnw"),
            root.join("mvnw.cmd"),
            root.join(".mvn")
                .join("wrapper")
                .join("maven-wrapper.properties"),
            root.join(".mvn").join("wrapper").join("maven-wrapper.jar"),
            root.join(".mvn").join("extensions.xml"),
            root.join(".mvn").join("maven.config"),
            root.join(".mvn").join("jvm.config"),
            root.join("WORKSPACE"),
            root.join("WORKSPACE.bazel"),
            root.join("MODULE.bazel"),
            root.join("BUILD"),
            root.join("BUILD.bazel"),
            root.join("some").join("pkg").join("BUILD"),
            root.join("some").join("pkg").join("BUILD.bazel"),
            root.join("tools").join("defs.bzl"),
            root.join(".nova")
                .join("apt-cache")
                .join("generated-roots.json"),
            root.join("libs.versions.toml"),
            root.join("dependencies.gradle"),
            root.join("dependencies.gradle.kts"),
            root.join("gradle").join("libs.versions.toml"),
            root.join("gradle").join("foo.versions.toml"),
            root.join("gradle").join("dependencies.gradle"),
            root.join("gradle").join("dependencies.gradle.kts"),
        ];

        for path in build_files {
            assert!(
                is_build_file(&path),
                "expected {} to be treated as a build file",
                path.display()
            );
            let event = NormalizedEvent::Modified(path.clone());
            assert_eq!(
                categorize_event(&config, &event),
                Some(ChangeCategory::Build),
                "expected {} to be categorized as Build",
                path.display()
            );
        }

        let non_build_files = [
            root.join("jvm.config"),
            // Wrapper jars must be in their canonical wrapper locations.
            root.join("gradle-wrapper.jar"),
            root.join(".mvn").join("maven-wrapper.jar"),
            // Custom version catalogs must live directly under `gradle/`.
            root.join("foo.versions.toml"),
            root.join(".gradle").join("dependencies.gradle"),
            root.join("build").join("dependencies.gradle"),
            root.join("target").join("dependencies.gradle"),
        ];

        for path in non_build_files {
            assert!(
                !is_build_file(&path),
                "expected {} not to be treated as a build file",
                path.display()
            );
            let event = NormalizedEvent::Modified(path.clone());
            assert_ne!(
                categorize_event(&config, &event),
                Some(ChangeCategory::Build),
                "expected {} not to be categorized as Build",
                path.display()
            );
        }
    }

    #[test]
    fn nova_generated_roots_snapshot_changes_are_build_changes() {
        let root = PathBuf::from("/tmp/workspace");
        let config = WatchConfig::new(root.clone());

        let path = root
            .join(".nova")
            .join("apt-cache")
            .join("generated-roots.json");

        assert!(
            is_build_file(&path),
            "expected {} to be treated as a build file",
            path.display()
        );

        let event = NormalizedEvent::Modified(path.clone());
        assert_eq!(
            categorize_event(&config, &event),
            Some(ChangeCategory::Build),
            "expected {} to be categorized as Build",
            path.display()
        );

        let non_build_files = [
            root.join("generated-roots.json"),
            root.join("apt-cache").join("generated-roots.json"),
            root.join(".nova").join("generated-roots.json"),
            root.join(".nova")
                .join("apt-cache")
                .join("generated-roots.json.bak"),
        ];

        for path in non_build_files {
            assert!(
                !is_build_file(&path),
                "expected {} to NOT be treated as a build file",
                path.display()
            );
        }
    }

    #[test]
    fn gradle_snapshot_file_is_a_build_file() {
        assert!(is_build_file(Path::new("/x/.nova/queries/gradle.json")));
    }

    #[test]
    fn gradle_snapshot_file_change_is_categorized_as_build() {
        let config = WatchConfig::new(PathBuf::from("/x"));
        let event = NormalizedEvent::Modified(PathBuf::from("/x/.nova/queries/gradle.json"));
        assert_eq!(
            categorize_event(&config, &event),
            Some(ChangeCategory::Build)
        );
    }

    #[test]
    fn java_edits_remain_source_changes() {
        let root = PathBuf::from("/tmp/workspace");
        let config = WatchConfig::new(root.clone());
        let path = root.join("Example.java");
        assert!(!is_build_file(&path));
        let event = NormalizedEvent::Modified(path);
        assert_eq!(
            categorize_event(&config, &event),
            Some(ChangeCategory::Source)
        );
    }

    #[test]
    fn java_files_outside_configured_roots_are_ignored() {
        let root = PathBuf::from("/tmp/workspace");
        let mut config = WatchConfig::new(root.clone());
        config.source_roots = vec![root.join("src/main/java")];

        let path = root.join("Scratch.java");
        let event = NormalizedEvent::Modified(path);
        assert_eq!(categorize_event(&config, &event), None);

        let in_root = root.join("src/main/java/Example.java");
        let event = NormalizedEvent::Modified(in_root);
        assert_eq!(
            categorize_event(&config, &event),
            Some(ChangeCategory::Source)
        );
    }

    #[test]
    fn module_info_java_changes_are_categorized_as_build() {
        let root = PathBuf::from("/tmp/workspace");
        let config = WatchConfig::new(root.clone());
        let module_info = root.join("src/module-info.java");

        assert_eq!(
            categorize_event(&config, &NormalizedEvent::Modified(module_info.clone())),
            Some(ChangeCategory::Build)
        );
        assert_eq!(
            categorize_event(&config, &NormalizedEvent::Created(module_info.clone())),
            Some(ChangeCategory::Build)
        );
        assert_eq!(
            categorize_event(&config, &NormalizedEvent::Deleted(module_info.clone())),
            Some(ChangeCategory::Build)
        );
    }

    #[test]
    fn module_info_java_moves_are_categorized_as_build() {
        let root = PathBuf::from("/tmp/workspace");
        let config = WatchConfig::new(root.clone());
        let module_info = root.join("src/module-info.java");
        let other = root.join("src/Other.java");

        assert_eq!(
            categorize_event(
                &config,
                &NormalizedEvent::Moved {
                    from: module_info.clone(),
                    to: other.clone(),
                }
            ),
            Some(ChangeCategory::Build)
        );
        assert_eq!(
            categorize_event(
                &config,
                &NormalizedEvent::Moved {
                    from: other,
                    to: module_info,
                }
            ),
            Some(ChangeCategory::Build)
        );
    }

    #[test]
    fn configured_root_with_dotdot_segments_matches_equivalent_event_path() {
        let config = WatchConfig::with_roots(
            PathBuf::from("/tmp/ws"),
            Vec::new(),
            vec![PathBuf::from("/tmp/ws/module/../gen")],
        );

        let event = NormalizedEvent::Modified(PathBuf::from("/tmp/ws/gen/A.java"));
        assert_eq!(categorize_event(&config, &event), Some(ChangeCategory::Source));
    }

    #[cfg(windows)]
    #[test]
    fn configured_root_drive_letter_case_does_not_affect_matching() {
        let config = WatchConfig::with_roots(
            PathBuf::from(r"c:\ws"),
            vec![PathBuf::from(r"c:\ws\src")],
            Vec::new(),
        );

        // Intentionally use the opposite drive-letter case from the configured root.
        let event = NormalizedEvent::Modified(PathBuf::from(r"C:\ws\src\A.java"));
        assert_eq!(categorize_event(&config, &event), Some(ChangeCategory::Source));
    }

    #[test]
    fn build_file_detection_uses_paths_relative_to_workspace_root() {
        // Regression test: `is_build_file` ignores `build/`, `target/`, and `.gradle/` directories
        // by scanning path components. If the workspace root itself lives under a directory named
        // `build` (e.g. `/home/user/build/my-project`), we still want to treat top-level Gradle
        // script plugins and version catalogs as build changes.
        let root = PathBuf::from("/tmp/build/workspace");
        let config = WatchConfig::new(root.clone());

        assert_eq!(
            categorize_event(&config, &NormalizedEvent::Modified(root.join("dependencies.gradle"))),
            Some(ChangeCategory::Build),
            "root-level Gradle script plugins should be treated as build changes"
        );
        assert_eq!(
            categorize_event(&config, &NormalizedEvent::Modified(root.join("libs.versions.toml"))),
            Some(ChangeCategory::Build),
            "root-level version catalogs should be treated as build changes"
        );

        // But build output directories inside the workspace should still be ignored.
        assert_ne!(
            categorize_event(
                &config,
                &NormalizedEvent::Modified(root.join("build/dependencies.gradle"))
            ),
            Some(ChangeCategory::Build),
            "files under workspace build/ should not be treated as build changes"
        );
    }
}
