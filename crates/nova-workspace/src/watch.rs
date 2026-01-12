use std::path::{Path, PathBuf};

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
    pub fn paths(&self) -> Vec<&Path> {
        match self {
            NormalizedEvent::Created(p)
            | NormalizedEvent::Modified(p)
            | NormalizedEvent::Deleted(p) => vec![p.as_path()],
            NormalizedEvent::Moved { from, to } => vec![from.as_path(), to.as_path()],
        }
    }
}

#[derive(Debug, Clone)]
pub struct WatchConfig {
    /// Workspace root (used to classify `.java` files).
    pub workspace_root: PathBuf,
    pub source_roots: Vec<PathBuf>,
    pub generated_source_roots: Vec<PathBuf>,
}

impl WatchConfig {
    pub fn new(workspace_root: PathBuf) -> Self {
        Self {
            workspace_root,
            source_roots: Vec::new(),
            generated_source_roots: Vec::new(),
        }
    }
}

pub fn categorize_event(config: &WatchConfig, event: &NormalizedEvent) -> Option<ChangeCategory> {
    for path in event.paths() {
        // `module-info.java` updates the JPMS module graph embedded in `ProjectConfig`. Treat it
        // like a build change so we reload the project config instead of only updating file
        // contents.
        if path
            .file_name()
            .is_some_and(|name| name == "module-info.java")
        {
            return Some(ChangeCategory::Build);
        }
        if is_build_file(path) {
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
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return false;
    };

    // Nova's generated-source roots discovery reads a snapshot under
    // `.nova/apt-cache/generated-roots.json`. Updates to this file should
    // trigger a project reload so newly discovered generated roots are watched.
    if name == "generated-roots.json"
        && path.ends_with(Path::new(".nova/apt-cache/generated-roots.json"))
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
        "mvnw" | "mvnw.cmd" => true,
        "maven-wrapper.properties" => {
            path.ends_with(Path::new(".mvn/wrapper/maven-wrapper.properties"))
        }
        "extensions.xml" => path.ends_with(Path::new(".mvn/extensions.xml")),
        "maven.config" => path.ends_with(Path::new(".mvn/maven.config")),
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
            root.join(".bazelrc"),
            root.join(".bazelrc.user"),
            root.join(".bazelversion"),
            root.join("MODULE.bazel.lock"),
            root.join("bazelisk.rc"),
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
            root.join("mvnw"),
            root.join("mvnw.cmd"),
            root.join(".mvn")
                .join("wrapper")
                .join("maven-wrapper.properties"),
            root.join(".mvn").join("extensions.xml"),
            root.join(".mvn").join("maven.config"),
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
}
