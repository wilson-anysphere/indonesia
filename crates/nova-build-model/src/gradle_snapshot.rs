use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Schema version for `.nova/queries/gradle.json`.
pub const GRADLE_SNAPSHOT_SCHEMA_VERSION: u32 = 1;

/// Relative path to the workspace-local Gradle snapshot file.
pub const GRADLE_SNAPSHOT_REL_PATH: &str = ".nova/queries/gradle.json";

/// Glob pattern matching the workspace-local Gradle snapshot file.
///
/// This is primarily used by build file watching logic (e.g. editor integrations) so changes to
/// the snapshot can trigger a reload.
pub const GRADLE_SNAPSHOT_GLOB: &str = "**/.nova/queries/gradle.json";

/// Workspace-local Gradle snapshot file (`.nova/queries/gradle.json`).
///
/// This file is written by `nova-build` after invoking Gradle and is read by `nova-project` to
/// enrich Gradle project metadata without invoking Gradle again.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GradleSnapshotFile {
    pub schema_version: u32,
    pub build_fingerprint: String,
    #[serde(default)]
    pub projects: Vec<GradleSnapshotProject>,
    #[serde(default)]
    pub java_compile_configs: BTreeMap<String, GradleSnapshotJavaCompileConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GradleSnapshotProject {
    pub path: String,
    pub project_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GradleSnapshotJavaCompileConfig {
    pub project_dir: PathBuf,
    #[serde(default)]
    pub compile_classpath: Vec<PathBuf>,
    #[serde(default)]
    pub test_classpath: Vec<PathBuf>,
    #[serde(default)]
    pub module_path: Vec<PathBuf>,
    #[serde(default)]
    pub main_source_roots: Vec<PathBuf>,
    #[serde(default)]
    pub test_source_roots: Vec<PathBuf>,
    pub main_output_dir: Option<PathBuf>,
    pub test_output_dir: Option<PathBuf>,
    pub source: Option<String>,
    pub target: Option<String>,
    pub release: Option<String>,
    #[serde(default)]
    pub enable_preview: bool,
}
