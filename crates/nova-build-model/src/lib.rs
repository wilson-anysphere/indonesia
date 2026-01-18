//! Shared project/build model types used across Nova build system integrations.

mod build_files;
mod generated_roots_snapshot;
mod gradle_snapshot;
pub mod groovy_scan;
mod model;
pub mod package;

pub use build_files::{
    collect_gradle_build_files, is_gradle_marker_root, parse_gradle_settings_included_builds,
    strip_gradle_comments, BuildFileFingerprint,
};
pub use generated_roots_snapshot::*;
pub use gradle_snapshot::*;
pub use model::*;
pub use package::{
    class_to_file_name, infer_source_root, is_valid_package_name, package_to_path, path_ends_with,
    validate_package_name, PackageNameError,
};

use std::path::{Path, PathBuf};

/// Canonical, build-system-agnostic project model type.
///
/// This is intentionally a type alias for now so we can keep the concrete model
/// accessible without additional wrapper indirection.
pub type ProjectModel = WorkspaceProjectModel;

/// Build-system-agnostic classpath buckets.
///
/// This matches the shape described in `instructions/build-systems.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classpath {
    /// Compile classpath (production dependencies).
    pub compile: Vec<ClasspathEntry>,
    /// Runtime classpath (may be identical to `compile` for now).
    pub runtime: Vec<ClasspathEntry>,
    /// Test classpath (includes test dependencies).
    pub test: Vec<ClasspathEntry>,
}

impl Classpath {
    pub fn empty() -> Self {
        Self {
            compile: Vec::new(),
            runtime: Vec::new(),
            test: Vec::new(),
        }
    }

    /// Best-effort union of classpath entries across all workspace modules.
    ///
    /// Entries are deduplicated deterministically and include:
    /// - module output directories (main/test) from [`WorkspaceModuleConfig::output_dirs`]
    /// - module-path + classpath dependency entries
    pub fn from_workspace_model_union(model: &WorkspaceProjectModel) -> Self {
        let mut modules: Vec<&WorkspaceModuleConfig> = model.modules.iter().collect();
        // Ensure deterministic union ordering even if upstream loaders don't provide a stable
        // module ordering.
        modules.sort_by(|a, b| a.id.cmp(&b.id));

        let mut compile_outputs = Vec::new();
        let mut test_outputs = Vec::new();
        let mut deps = Vec::new();

        for module in modules {
            for out in &module.output_dirs {
                let entry = ClasspathEntry {
                    kind: ClasspathEntryKind::Directory,
                    path: out.path.clone(),
                };
                match out.kind {
                    OutputDirKind::Main => {
                        compile_outputs.push(entry.clone());
                        test_outputs.push(entry);
                    }
                    OutputDirKind::Test => {
                        test_outputs.push(entry);
                    }
                }
            }

            deps.extend(module.module_path.iter().cloned());
            deps.extend(module.classpath.iter().cloned());
        }

        dedup_classpath_entries_preserve_order(&mut compile_outputs);
        dedup_classpath_entries_preserve_order(&mut test_outputs);
        dedup_classpath_entries_preserve_order(&mut deps);

        let mut compile = compile_outputs;
        compile.extend(deps.clone());
        dedup_classpath_entries_preserve_order(&mut compile);

        let runtime = compile.clone();

        let mut test = test_outputs;
        test.extend(deps);
        dedup_classpath_entries_preserve_order(&mut test);

        Self {
            compile,
            runtime,
            test,
        }
    }
}

impl Default for Classpath {
    fn default() -> Self {
        Self::empty()
    }
}

fn dedup_classpath_entries_preserve_order(entries: &mut Vec<ClasspathEntry>) {
    use std::collections::HashSet;

    let mut seen: HashSet<ClasspathEntry> = HashSet::new();
    entries.retain(|entry| seen.insert(entry.clone()));
}

#[cfg(test)]
mod classpath_tests {
    use super::*;

    #[test]
    fn classpath_union_includes_output_dirs_and_dedups() {
        let out_a_main = PathBuf::from("/workspace/a/target/classes");
        let out_a_test = PathBuf::from("/workspace/a/target/test-classes");
        let out_b_main = PathBuf::from("/workspace/b/target/classes");

        let dep_named = ClasspathEntry {
            kind: ClasspathEntryKind::Jar,
            path: PathBuf::from("/deps/named-module.jar"),
        };
        let dep_plain = ClasspathEntry {
            kind: ClasspathEntryKind::Jar,
            path: PathBuf::from("/deps/dep.jar"),
        };

        let module_a = WorkspaceModuleConfig {
            id: "a".to_string(),
            name: "a".to_string(),
            root: PathBuf::from("/workspace/a"),
            build_id: WorkspaceModuleBuildId::Maven {
                module_path: ":a".to_string(),
                gav: Some(MavenGav {
                    group_id: "com.example".to_string(),
                    artifact_id: "a".to_string(),
                    version: Some("1.0.0".to_string()),
                }),
            },
            language_level: ModuleLanguageLevel {
                level: JavaLanguageLevel::from_java_config(JavaConfig::default()),
                provenance: LanguageLevelProvenance::Default,
            },
            source_roots: Vec::new(),
            output_dirs: vec![
                OutputDir {
                    kind: OutputDirKind::Main,
                    path: out_a_main.clone(),
                },
                OutputDir {
                    kind: OutputDirKind::Test,
                    path: out_a_test.clone(),
                },
            ],
            module_path: vec![dep_named.clone()],
            classpath: vec![dep_plain.clone()],
            dependencies: Vec::new(),
        };

        let module_b = WorkspaceModuleConfig {
            id: "b".to_string(),
            name: "b".to_string(),
            root: PathBuf::from("/workspace/b"),
            build_id: WorkspaceModuleBuildId::Maven {
                module_path: ":b".to_string(),
                gav: Some(MavenGav {
                    group_id: "com.example".to_string(),
                    artifact_id: "b".to_string(),
                    version: Some("1.0.0".to_string()),
                }),
            },
            language_level: ModuleLanguageLevel {
                level: JavaLanguageLevel::from_java_config(JavaConfig::default()),
                provenance: LanguageLevelProvenance::Default,
            },
            source_roots: Vec::new(),
            output_dirs: vec![OutputDir {
                kind: OutputDirKind::Main,
                path: out_b_main.clone(),
            }],
            // Duplicate dependency entry to ensure we dedup across modules.
            module_path: vec![dep_named.clone()],
            classpath: Vec::new(),
            dependencies: Vec::new(),
        };

        // Intentionally pass modules out of order to verify deterministic sorting by id.
        let model = WorkspaceProjectModel::new(
            PathBuf::from("/workspace"),
            BuildSystem::Maven,
            JavaConfig::default(),
            vec![module_b, module_a],
            Vec::new(),
        );

        let cp = Classpath::from_workspace_model_union(&model);

        // Output dirs should come first (deterministically: module `a` then `b`).
        assert_eq!(
            cp.compile
                .iter()
                .take(2)
                .map(|e| e.path.clone())
                .collect::<Vec<_>>(),
            vec![out_a_main.clone(), out_b_main.clone()]
        );
        assert!(cp.compile.contains(&ClasspathEntry {
            kind: ClasspathEntryKind::Jar,
            path: dep_named.path.clone()
        }));
        assert!(cp.compile.contains(&ClasspathEntry {
            kind: ClasspathEntryKind::Jar,
            path: dep_plain.path.clone()
        }));

        // Test classpath should include main + test output dirs.
        assert!(cp.test.contains(&ClasspathEntry {
            kind: ClasspathEntryKind::Directory,
            path: out_a_main.clone(),
        }));
        assert!(cp.test.contains(&ClasspathEntry {
            kind: ClasspathEntryKind::Directory,
            path: out_a_test.clone(),
        }));

        // Runtime should match compile for now.
        assert_eq!(cp.runtime, cp.compile);

        // Dedup: dep_named appears only once in compile.
        let dep_named_count = cp
            .compile
            .iter()
            .filter(|e| e.kind == dep_named.kind && e.path == dep_named.path)
            .count();
        assert_eq!(dep_named_count, 1);
    }
}

/// Lightweight file path matcher for build file watching.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PathPattern {
    /// Matches a file by its exact file name (no directory components).
    ExactFileName(&'static str),
    /// Matches a path via a glob pattern (syntax is consumer-defined).
    Glob(&'static str),
}

#[derive(Debug, thiserror::Error)]
pub enum BuildSystemError {
    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Message(String),

    #[error(transparent)]
    Other(#[from] Box<dyn std::error::Error + Send + Sync>),
}

impl BuildSystemError {
    pub fn other(err: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Other(Box::new(err))
    }

    pub fn message(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }
}

/// Object-safe build-system backend abstraction.
///
/// This is defined in `nova-build-model` under a distinct name to avoid colliding with the
/// `BuildSystem` enum in the project model. Higher-level crates re-export it under the
/// public name `BuildSystem`.
pub trait BuildSystemBackend: Send + Sync {
    /// Detect if this build system is used for the workspace rooted at `root`.
    fn detect(&self, root: &Path) -> bool;

    /// Parse/build a project model for the workspace rooted at `root`.
    fn parse_project(&self, root: &Path) -> Result<ProjectModel, BuildSystemError>;

    /// Resolve the workspace dependencies into a classpath.
    fn resolve_classpath(&self, project: &ProjectModel) -> Result<Classpath, BuildSystemError>;

    /// Return path patterns for build-related files that should trigger reloads.
    fn watch_files(&self) -> Vec<PathPattern>;
}

/// Returns `true` if the given directory looks like a Bazel workspace root.
///
/// A Bazel workspace root is identified by the presence of one of:
/// - `WORKSPACE`
/// - `WORKSPACE.bazel`
/// - `MODULE.bazel`
pub fn is_bazel_workspace(root: &Path) -> bool {
    ["WORKSPACE", "WORKSPACE.bazel", "MODULE.bazel"]
        .iter()
        .any(|marker| root.join(marker).is_file())
}

/// Walk upwards from `start` to find the Bazel workspace root.
///
/// `start` may be either a directory or a file path within a workspace.
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
