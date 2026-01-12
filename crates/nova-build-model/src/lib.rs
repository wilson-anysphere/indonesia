//! Shared project/build model types used across Nova build system integrations.

mod model;

pub use model::*;

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
    /// Entries are deduplicated and sorted deterministically.
    pub fn from_workspace_model_union(model: &WorkspaceProjectModel) -> Self {
        let mut entries = Vec::new();
        for module in &model.modules {
            entries.extend(module.module_path.iter().cloned());
            entries.extend(module.classpath.iter().cloned());
        }

        sort_dedup_classpath_entries(&mut entries);

        Self {
            compile: entries.clone(),
            runtime: entries.clone(),
            test: entries,
        }
    }
}

impl Default for Classpath {
    fn default() -> Self {
        Self::empty()
    }
}

fn sort_dedup_classpath_entries(entries: &mut Vec<ClasspathEntry>) {
    entries.sort_by(|a, b| a.path.cmp(&b.path).then(a.kind.cmp(&b.kind)));
    entries.dedup_by(|a, b| a.kind == b.kind && a.path == b.path);
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
    let mut dir = if start.is_file() { start.parent()? } else { start };

    loop {
        if is_bazel_workspace(dir) {
            return Some(dir.to_path_buf());
        }
        dir = dir.parent()?;
    }
}
