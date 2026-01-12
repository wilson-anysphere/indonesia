use notify::EventKind;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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

    if name == "pom.xml"
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

#[derive(Debug, Default)]
pub struct EventNormalizer {
    pending_renames: VecDeque<(Instant, PathBuf)>,
}

impl EventNormalizer {
    pub fn new() -> Self {
        Self {
            pending_renames: VecDeque::new(),
        }
    }

    pub fn push(&mut self, event: notify::Event, now: Instant) -> Vec<NormalizedEvent> {
        self.gc_pending(now);

        use notify::event::{ModifyKind, RenameMode};

        match event.kind {
            EventKind::Create(_) => event
                .paths
                .into_iter()
                .map(NormalizedEvent::Created)
                .collect(),
            EventKind::Remove(_) => event
                .paths
                .into_iter()
                .map(NormalizedEvent::Deleted)
                .collect(),
            EventKind::Modify(ModifyKind::Data(_))
            | EventKind::Modify(ModifyKind::Metadata(_))
            | EventKind::Modify(ModifyKind::Other)
            | EventKind::Modify(ModifyKind::Any) => event
                .paths
                .into_iter()
                .map(NormalizedEvent::Modified)
                .collect(),
            EventKind::Modify(ModifyKind::Name(rename_mode)) => match rename_mode {
                RenameMode::Both => paths_to_moves(event.paths),
                RenameMode::From => {
                    for path in event.paths {
                        self.pending_renames.push_back((now, path));
                    }
                    Vec::new()
                }
                RenameMode::To => {
                    let mut out = Vec::new();
                    for to in event.paths {
                        if let Some((_, from)) = self.pending_renames.pop_front() {
                            out.push(NormalizedEvent::Moved { from, to });
                        } else {
                            out.push(NormalizedEvent::Created(to));
                        }
                    }
                    out
                }
                // Unknown rename variants: treat as modified.
                RenameMode::Any | RenameMode::Other => event
                    .paths
                    .into_iter()
                    .map(NormalizedEvent::Modified)
                    .collect(),
            },
            // Some backends report a rename as a "modify" without further detail.
            _ => event
                .paths
                .into_iter()
                .map(NormalizedEvent::Modified)
                .collect(),
        }
    }

    fn gc_pending(&mut self, now: Instant) {
        const MAX_AGE: Duration = Duration::from_secs(2);
        while let Some((t, _)) = self.pending_renames.front() {
            if now.duration_since(*t) <= MAX_AGE {
                break;
            }
            self.pending_renames.pop_front();
        }

        // Bound memory for rename storms.
        while self.pending_renames.len() > 512 {
            self.pending_renames.pop_front();
        }
    }
}

fn paths_to_moves(mut paths: Vec<PathBuf>) -> Vec<NormalizedEvent> {
    let mut out = Vec::new();
    while paths.len() >= 2 {
        let from = paths.remove(0);
        let to = paths.remove(0);
        out.push(NormalizedEvent::Moved { from, to });
    }
    // Leftover path: treat as modified.
    if let Some(path) = paths.pop() {
        out.push(NormalizedEvent::Modified(path));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{ModifyKind, RenameMode};

    #[test]
    fn normalize_rename_from_to_into_move() {
        let mut normalizer = EventNormalizer::new();
        let t0 = Instant::now();

        let from = PathBuf::from("/tmp/A.java");
        let to = PathBuf::from("/tmp/B.java");

        let ev_from = notify::Event {
            kind: EventKind::Modify(ModifyKind::Name(RenameMode::From)),
            paths: vec![from.clone()],
            attrs: Default::default(),
        };
        let ev_to = notify::Event {
            kind: EventKind::Modify(ModifyKind::Name(RenameMode::To)),
            paths: vec![to.clone()],
            attrs: Default::default(),
        };

        assert!(normalizer.push(ev_from, t0).is_empty());
        assert_eq!(
            normalizer.push(ev_to, t0),
            vec![NormalizedEvent::Moved { from, to }]
        );
    }

    #[test]
    fn normalize_create_and_remove() {
        let mut normalizer = EventNormalizer::new();
        let t0 = Instant::now();
        let p = PathBuf::from("/tmp/A.java");

        let create = notify::Event {
            kind: EventKind::Create(notify::event::CreateKind::File),
            paths: vec![p.clone()],
            attrs: Default::default(),
        };
        let remove = notify::Event {
            kind: EventKind::Remove(notify::event::RemoveKind::File),
            paths: vec![p.clone()],
            attrs: Default::default(),
        };

        assert_eq!(
            normalizer.push(create, t0),
            vec![NormalizedEvent::Created(p.clone())]
        );
        assert_eq!(
            normalizer.push(remove, t0),
            vec![NormalizedEvent::Deleted(p)]
        );
    }

    #[test]
    fn build_file_changes_are_categorized_as_build() {
        let root = PathBuf::from("/tmp/workspace");
        let config = WatchConfig::new(root.clone());

        let build_files = [
            root.join("pom.xml"),
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
}
