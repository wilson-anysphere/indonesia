use std::path::{Path, PathBuf};

use nova_vfs::FileChange;

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

#[derive(Debug, Clone)]
pub struct WatchConfig {
    /// Workspace root (used to classify `.java` files).
    pub workspace_root: PathBuf,
    pub source_roots: Vec<PathBuf>,
    pub generated_source_roots: Vec<PathBuf>,
    /// Build-system module roots (`ProjectConfig.modules[*].root`).
    ///
    /// This is used by higher layers to select file-watcher roots, ensuring build file changes in
    /// modules outside the workspace root (e.g. Maven `<modules>` entries like `../common`) still
    /// trigger project reloads.
    pub module_roots: Vec<PathBuf>,
    /// The effective Nova config file path for this workspace, if any.
    ///
    /// This may live outside `workspace_root` when `NOVA_CONFIG_PATH` is set or when
    /// callers explicitly pass a config path (e.g. `nova-lsp --config <path>`).
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
                .map(normalize_watch_path)
                .collect(),
            generated_source_roots: generated_source_roots
                .into_iter()
                .map(normalize_watch_path)
                .collect(),
            module_roots: Vec::new(),
            nova_config_path: None,
        }
    }
}

pub fn categorize_event(config: &WatchConfig, change: &FileChange) -> Option<ChangeCategory> {
    // Normalize event paths before categorization so drive-letter case (`c:` vs `C:` on Windows)
    // and dot segments (`a/../b`) don't affect source-root/build-file matching.
    //
    // This does **not** resolve symlinks; it is purely lexical normalization via `nova-vfs`.
    let paths: Vec<PathBuf> = change
        .paths()
        .filter_map(|path| path.as_local_path())
        .map(|path| normalize_watch_path(path.to_path_buf()))
        .collect();

    for path in &paths {
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
        let rel = path.strip_prefix(&config.workspace_root).unwrap_or_else(|_| {
            config
                .module_roots
                .iter()
                .chain(config.source_roots.iter())
                .chain(config.generated_source_roots.iter())
                .find_map(|root| path.strip_prefix(root).ok())
                .unwrap_or(path.as_path())
        });
        if is_build_file(rel) {
            return Some(ChangeCategory::Build);
        }
    }

    let has_configured_roots =
        !config.source_roots.is_empty() || !config.generated_source_roots.is_empty();

    let is_in_source_tree = |path: &Path| {
        if has_configured_roots {
            is_within_any(path, &config.source_roots)
                || is_within_any(path, &config.generated_source_roots)
        } else {
            // Fall back to treating the entire workspace root as a source root when we don't have
            // more specific roots (e.g. simple projects).
            path.starts_with(&config.workspace_root)
        }
    };

    // We primarily index Java sources.
    for path in &paths {
        if path.extension().and_then(|s| s.to_str()) == Some("java") && is_in_source_tree(path) {
            return Some(ChangeCategory::Source);
        }
    }

    // Directory-level watcher events (rename/move/delete) can arrive without per-file events.
    // Treat directory events inside the source tree as Source changes so the workspace engine can
    // expand them into file-level operations without allocating bogus `FileId`s.
    for path in &paths {
        if path.is_dir() && is_in_source_tree(path) {
            return Some(ChangeCategory::Source);
        }
    }

    // Deleted directories no longer exist, so `is_dir()` can't detect them. Heuristic: if the
    // deleted/moved path has no extension and lives under the source tree, pass it through so the
    // workspace engine can decide whether it corresponds to a tracked directory.
    match change {
        FileChange::Deleted { path } => {
            let Some(path) = path.as_local_path() else {
                return None;
            };
            let path = normalize_watch_path(path.to_path_buf());
            if path.extension().is_none() && is_in_source_tree(&path) {
                return Some(ChangeCategory::Source);
            }
        }
        FileChange::Moved { from, to } => {
            let (Some(from), Some(to)) = (from.as_local_path(), to.as_local_path()) else {
                return None;
            };
            let from = normalize_watch_path(from.to_path_buf());
            let to = normalize_watch_path(to.to_path_buf());
            if (from.extension().is_none() && is_in_source_tree(&from))
                || (to.extension().is_none() && is_in_source_tree(&to))
            {
                return Some(ChangeCategory::Source);
            }
        }
        _ => {}
    }

    None
}

fn is_within_any(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| path.starts_with(root))
}

fn is_in_noisy_dir(path: &Path) -> bool {
    path.components().any(|c| {
        let component = c.as_os_str();
        if component == std::ffi::OsStr::new(".git")
            || component == std::ffi::OsStr::new(".gradle")
            || component == std::ffi::OsStr::new(".idea")
            || component == std::ffi::OsStr::new(".nova")
            || component == std::ffi::OsStr::new("build")
            || component == std::ffi::OsStr::new("target")
            || component == std::ffi::OsStr::new("node_modules")
            || component == std::ffi::OsStr::new("bazel-out")
            || component == std::ffi::OsStr::new("bazel-bin")
            || component == std::ffi::OsStr::new("bazel-testlogs")
        {
            return true;
        }

        // Best-effort skip for Bazel output trees. Bazel creates `bazel-<workspace>` symlinks at
        // the workspace root, but in practice these entries can appear at arbitrary depths in
        // large repos, so ignore them wherever they show up.
        component
            .to_str()
            .is_some_and(|component| component.starts_with("bazel-"))
    })
}

pub fn is_build_file(path: &Path) -> bool {
    // Some Nova build integrations write snapshot state under `.nova/queries`. These affect
    // generated root discovery and/or classpath resolution, so treat them like build files.
    if path.ends_with(nova_build_model::GRADLE_SNAPSHOT_REL_PATH) {
        return true;
    }

    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return false;
    };

    // Nova's generated-source roots discovery reads a snapshot under
    // `.nova/apt-cache/generated-roots.json`. Updates to this file should
    // trigger a project reload so newly discovered generated roots are watched.
    if name == "generated-roots.json"
        && path.ends_with(
            &Path::new(".nova")
                .join("apt-cache")
                .join("generated-roots.json"),
        )
    {
        return true;
    }

    // Nova internal config is stored under `.nova/config.toml`. This is primarily a legacy
    // fallback for `nova_config::discover_config_path`, but still needs to be watched so changes
    // take effect without restarting.
    if name == "config.toml" && path.ends_with(&Path::new(".nova").join("config.toml")) {
        return true;
    }

    // Ignore build markers under commonly noisy directories (e.g. Bazel output trees). This
    // mirrors `nova-build` build-file fingerprinting behavior so that generated / cached files do
    // not trigger workspace reloads.
    if is_in_noisy_dir(path) {
        return false;
    }

    // Gradle script plugins can influence dependencies and tasks.
    if name.ends_with(".gradle") || name.ends_with(".gradle.kts") {
        return true;
    }

    // Gradle version catalogs can define dependency versions.
    if name.ends_with(".versions.toml") {
        return true;
    }

    // Gradle dependency locking can change resolved classpaths without modifying build scripts,
    // so treat dependency lockfiles as build triggers.
    //
    // Patterns:
    // - `gradle.lockfile` at any depth.
    // - `*.lockfile` under any `dependency-locks/` directory (covers Gradle's default
    //   `gradle/dependency-locks/` location).
    if name == "gradle.lockfile" {
        return true;
    }
    if name.ends_with(".lockfile")
        && path.ancestors().any(|dir| {
            dir.file_name()
                .is_some_and(|name| name == "dependency-locks")
        })
    {
        return true;
    }

    // Bazel BSP server discovery uses `.bsp/*.json` connection files (optional).
    if path
        .parent()
        .and_then(|p| p.file_name())
        .is_some_and(|dir| dir == ".bsp")
        && path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
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
    use nova_vfs::VfsPath;

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
            root.join(nova_build_model::GRADLE_SNAPSHOT_REL_PATH),
            root.join("gradle.lockfile"),
            root.join("gradle")
                .join("dependency-locks")
                .join("compileClasspath.lockfile"),
            root.join("dependency-locks")
                .join("compileClasspath.lockfile"),
            root.join(".bsp").join("server.json"),
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
            root.join("gradle")
                .join("wrapper")
                .join("gradle-wrapper.jar"),
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
            root.join("libs.versions.toml"),
            root.join("deps.versions.toml"),
            root.join("dependencies.gradle"),
            root.join("dependencies.gradle.kts"),
            root.join("gradle").join("libs.versions.toml"),
            root.join("gradle").join("deps.versions.toml"),
            root.join("gradle").join("dependencies.gradle"),
            root.join("gradle").join("dependencies.gradle.kts"),
            root.join("gradle.lockfile"),
            root.join("gradle")
                .join("dependency-locks")
                .join("compileClasspath.lockfile"),
            root.join("dependency-locks").join("custom.lockfile"),
        ];

        for path in build_files {
            let rel = path.strip_prefix(&root).unwrap_or(&path);
            assert!(
                is_build_file(rel),
                "expected {} to be treated as a build file",
                path.display()
            );
            let event = FileChange::Modified {
                path: VfsPath::local(path.clone()),
            };
            assert_eq!(
                categorize_event(&config, &event),
                Some(ChangeCategory::Build),
                "expected {} to be categorized as Build",
                path.display()
            );
        }

        let non_build_files = [
            root.join("jvm.config"),
            root.join(".bsp").join("server.txt"),
            root.join("foo.lockfile"),
            root.join(".gradle").join("gradle.lockfile"),
            // Wrapper jars must be in their canonical wrapper locations.
            root.join("gradle-wrapper.jar"),
            root.join(".mvn").join("maven-wrapper.jar"),
            // Files under build output / cache dirs should not be treated as build inputs.
            root.join(".gradle").join("dependencies.gradle"),
            root.join(".gradle").join("deps.versions.toml"),
            root.join("build").join("dependencies.gradle"),
            root.join("build").join("deps.versions.toml"),
            root.join("build").join("gradle.lockfile"),
            root.join("target").join("dependencies.gradle"),
            root.join("target").join("deps.versions.toml"),
            root.join("target").join("gradle.lockfile"),
            root.join("target")
                .join("dependency-locks")
                .join("compileClasspath.lockfile"),
            root.join(".gradle").join("gradle.lockfile"),
            root.join("build")
                .join("dependency-locks")
                .join("compileClasspath.lockfile"),
            root.join("foo.lockfile"),
            // Ignore build markers in irrelevant directories.
            root.join("node_modules").join("pom.xml"),
            root.join("node_modules").join("build.gradle"),
            root.join("node_modules").join("deps.versions.toml"),
            root.join("node_modules").join("foo").join("build.gradle"),
            root.join("bazel-out").join("pom.xml"),
            root.join("bazel-out").join("foo").join("BUILD"),
            root.join("bazel-bin").join("build.gradle"),
            root.join("bazel-bin").join("foo").join("BUILD.bazel"),
            root.join("bazel-testlogs").join("pom.xml"),
            root.join("bazel-testlogs").join("foo").join("rules.bzl"),
            root.join("bazel-myworkspace").join("pom.xml"),
            root.join("bazel-myws").join("foo").join("WORKSPACE"),
        ];

        for path in non_build_files {
            let rel = path.strip_prefix(&root).unwrap_or(&path);
            assert!(
                !is_build_file(rel),
                "expected {} not to be treated as a build file",
                path.display()
            );
            let event = FileChange::Modified {
                path: VfsPath::local(path.clone()),
            };
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

        let event = FileChange::Modified {
            path: VfsPath::local(path.clone()),
        };
        assert_eq!(
            categorize_event(&config, &event),
            Some(ChangeCategory::Build),
            "expected {} to be categorized as Build",
            path.display()
        );

        assert_eq!(
            categorize_event(
                &config,
                &FileChange::Created {
                    path: VfsPath::local(path.clone())
                }
            ),
            Some(ChangeCategory::Build),
            "expected {} to be categorized as Build on create",
            path.display()
        );
        assert_eq!(
            categorize_event(
                &config,
                &FileChange::Deleted {
                    path: VfsPath::local(path.clone())
                }
            ),
            Some(ChangeCategory::Build),
            "expected {} to be categorized as Build on delete",
            path.display()
        );

        let other = root.join("Other.java");
        assert_eq!(
            categorize_event(
                &config,
                &FileChange::Moved {
                    from: VfsPath::local(path.clone()),
                    to: VfsPath::local(other.clone())
                }
            ),
            Some(ChangeCategory::Build),
            "expected move from {} to be categorized as Build",
            path.display()
        );
        assert_eq!(
            categorize_event(
                &config,
                &FileChange::Moved {
                    from: VfsPath::local(other),
                    to: VfsPath::local(path.clone())
                }
            ),
            Some(ChangeCategory::Build),
            "expected move to {} to be categorized as Build",
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
        let path = Path::new("/x").join(nova_build_model::GRADLE_SNAPSHOT_REL_PATH);
        assert!(is_build_file(&path));
    }

    #[test]
    fn gradle_snapshot_file_change_is_categorized_as_build() {
        let config = WatchConfig::new(PathBuf::from("/x"));
        let event = FileChange::Modified {
            path: VfsPath::local(PathBuf::from("/x").join(nova_build_model::GRADLE_SNAPSHOT_REL_PATH)),
        };
        assert_eq!(
            categorize_event(&config, &event),
            Some(ChangeCategory::Build)
        );
    }

    #[test]
    fn module_info_java_is_treated_as_build_file() {
        let root = PathBuf::from("/tmp/workspace");
        let module_info = root.join("src/main/java/module-info.java");
        assert!(is_build_file(&module_info));
    }

    #[test]
    fn module_info_java_change_is_categorized_as_build() {
        let root = PathBuf::from("/tmp/workspace");
        let config = WatchConfig::new(root.clone());
        let module_info = root.join("src/main/java/module-info.java");
        let event = FileChange::Modified {
            path: VfsPath::local(module_info),
        };
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
        let event = FileChange::Modified {
            path: VfsPath::local(path),
        };
        assert_eq!(
            categorize_event(&config, &event),
            Some(ChangeCategory::Source)
        );
    }

    #[test]
    fn java_changes_under_generated_roots_in_bazel_outputs_remain_source_changes() {
        let root = PathBuf::from("/tmp/workspace");
        let mut config = WatchConfig::new(root.clone());
        config.generated_source_roots = vec![root.join("bazel-out").join("gen-src")];

        let path = root
            .join("bazel-out")
            .join("gen-src")
            .join("com")
            .join("example")
            .join("Generated.java");

        assert!(
            !is_build_file(&path),
            "expected {} not to be treated as a build file",
            path.display()
        );

        let event = FileChange::Modified {
            path: VfsPath::local(path),
        };
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
        let event = FileChange::Modified {
            path: VfsPath::local(path),
        };
        assert_eq!(categorize_event(&config, &event), None);

        let in_root = root.join("src/main/java/Example.java");
        let event = FileChange::Modified {
            path: VfsPath::local(in_root),
        };
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
            categorize_event(
                &config,
                &FileChange::Modified {
                    path: VfsPath::local(module_info.clone())
                }
            ),
            Some(ChangeCategory::Build)
        );
        assert_eq!(
            categorize_event(
                &config,
                &FileChange::Created {
                    path: VfsPath::local(module_info.clone())
                }
            ),
            Some(ChangeCategory::Build)
        );
        assert_eq!(
            categorize_event(
                &config,
                &FileChange::Deleted {
                    path: VfsPath::local(module_info.clone())
                }
            ),
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
                &FileChange::Moved {
                    from: VfsPath::local(module_info.clone()),
                    to: VfsPath::local(other.clone())
                }
            ),
            Some(ChangeCategory::Build)
        );
        assert_eq!(
            categorize_event(
                &config,
                &FileChange::Moved {
                    from: VfsPath::local(other),
                    to: VfsPath::local(module_info)
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

        let event = FileChange::Modified {
            path: VfsPath::local(PathBuf::from("/tmp/ws/gen/A.java")),
        };
        assert_eq!(categorize_event(&config, &event), Some(ChangeCategory::Source));
    }

    #[test]
    fn event_paths_with_dotdot_segments_match_configured_roots() {
        let config = WatchConfig::with_roots(
            PathBuf::from("/tmp/ws"),
            Vec::new(),
            vec![PathBuf::from("/tmp/ws/gen")],
        );

        let event = FileChange::Modified {
            // Intentionally preserve `..` segments to ensure the categorizer's lexical
            // normalization logic is exercised.
            path: VfsPath::Local(PathBuf::from("/tmp/ws/module/../gen/A.java")),
        };
        assert_eq!(categorize_event(&config, &event), Some(ChangeCategory::Source));
    }

    #[test]
    fn workspace_root_with_dotdot_segments_matches_event_path_when_no_configured_roots() {
        let config = WatchConfig::new(PathBuf::from("/tmp/ws/module/.."));
        let event = FileChange::Modified {
            path: VfsPath::local(PathBuf::from("/tmp/ws/A.java")),
        };
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
        let event = FileChange::Modified {
            path: VfsPath::local(PathBuf::from(r"C:\ws\src\A.java")),
        };
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
            categorize_event(
                &config,
                &FileChange::Modified {
                    path: VfsPath::local(root.join("dependencies.gradle"))
                }
            ),
            Some(ChangeCategory::Build),
            "root-level Gradle script plugins should be treated as build changes"
        );
        assert_eq!(
            categorize_event(
                &config,
                &FileChange::Modified {
                    path: VfsPath::local(root.join("libs.versions.toml"))
                }
            ),
            Some(ChangeCategory::Build),
            "root-level version catalogs should be treated as build changes"
        );

        // But build output directories inside the workspace should still be ignored.
        assert_ne!(
            categorize_event(
                &config,
                &FileChange::Modified {
                    path: VfsPath::local(root.join("build/dependencies.gradle"))
                }
            ),
            Some(ChangeCategory::Build),
            "files under workspace build/ should not be treated as build changes"
        );
    }

    #[test]
    fn custom_nova_config_path_is_categorized_as_build() {
        let workspace_root = PathBuf::from("/tmp/workspace");
        let candidates = [
            workspace_root.join("custom-config.toml"),
            PathBuf::from("/tmp/custom/nova-config.external.toml"),
        ];

        for config_path in candidates {
            assert!(
                !is_build_file(&config_path),
                "test precondition: config path should not match standard build file names"
            );

            let mut config = WatchConfig::new(workspace_root.clone());
            config.nova_config_path = Some(config_path.clone());

            let event = FileChange::Modified {
                path: VfsPath::local(config_path),
            };
            assert_eq!(
                categorize_event(&config, &event),
                Some(ChangeCategory::Build)
            );
        }
    }
}
