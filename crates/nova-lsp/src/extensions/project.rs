use crate::{NovaLspError, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectConfigurationParams {
    /// Workspace root on disk.
    ///
    /// Clients should prefer `projectRoot` (camelCase). `root` is accepted as an
    /// alias for parity with other Nova extension endpoints.
    #[serde(alias = "root")]
    pub project_root: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BuildSystemKind {
    Maven,
    Gradle,
    Bazel,
    Simple,
}

impl From<nova_project::BuildSystem> for BuildSystemKind {
    fn from(value: nova_project::BuildSystem) -> Self {
        match value {
            nova_project::BuildSystem::Maven => Self::Maven,
            nova_project::BuildSystem::Gradle => Self::Gradle,
            nova_project::BuildSystem::Bazel => Self::Bazel,
            nova_project::BuildSystem::Simple => Self::Simple,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceRootKind {
    Main,
    Test,
}

impl From<nova_project::SourceRootKind> for SourceRootKind {
    fn from(value: nova_project::SourceRootKind) -> Self {
        match value {
            nova_project::SourceRootKind::Main => Self::Main,
            nova_project::SourceRootKind::Test => Self::Test,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceRootOrigin {
    Source,
    Generated,
}

impl From<nova_project::SourceRootOrigin> for SourceRootOrigin {
    fn from(value: nova_project::SourceRootOrigin) -> Self {
        match value {
            nova_project::SourceRootOrigin::Source => Self::Source,
            nova_project::SourceRootOrigin::Generated => Self::Generated,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceRootEntry {
    pub kind: SourceRootKind,
    pub origin: SourceRootOrigin,
    pub path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClasspathEntryKind {
    Directory,
    Jar,
}

impl From<nova_project::ClasspathEntryKind> for ClasspathEntryKind {
    fn from(value: nova_project::ClasspathEntryKind) -> Self {
        match value {
            nova_project::ClasspathEntryKind::Directory => Self::Directory,
            nova_project::ClasspathEntryKind::Jar => Self::Jar,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClasspathEntry {
    pub kind: ClasspathEntryKind,
    pub path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputDirKind {
    Main,
    Test,
}

impl From<nova_project::OutputDirKind> for OutputDirKind {
    fn from(value: nova_project::OutputDirKind) -> Self {
        match value {
            nova_project::OutputDirKind::Main => Self::Main,
            nova_project::OutputDirKind::Test => Self::Test,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutputDirEntry {
    pub kind: OutputDirKind,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModuleEntry {
    pub name: String,
    pub root: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JavaConfigEntry {
    pub source: u16,
    pub target: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DependencyEntry {
    pub group_id: String,
    pub artifact_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classifier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "type")]
    pub type_: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectConfigurationResponse {
    pub schema_version: u32,
    pub workspace_root: String,
    pub build_system: BuildSystemKind,
    pub java: JavaConfigEntry,
    pub modules: Vec<ModuleEntry>,
    pub source_roots: Vec<SourceRootEntry>,
    pub classpath: Vec<ClasspathEntry>,
    pub module_path: Vec<ClasspathEntry>,
    pub output_dirs: Vec<OutputDirEntry>,
    pub dependencies: Vec<DependencyEntry>,
}

pub fn handle_project_configuration(params: serde_json::Value) -> Result<serde_json::Value> {
    let params: ProjectConfigurationParams = serde_json::from_value(params)
        .map_err(|err| NovaLspError::InvalidParams(crate::sanitize_serde_json_error(&err)))?;

    if params.project_root.trim().is_empty() {
        return Err(NovaLspError::InvalidParams(
            "`projectRoot` must not be empty".to_string(),
        ));
    }

    let root = PathBuf::from(&params.project_root);
    let config = nova_project::load_project(&root)
        .map_err(|err| {
            NovaLspError::Internal(format!(
                "failed to load project: {}",
                crate::sanitize_error_message(&err)
            ))
        })?;

    let resp = ProjectConfigurationResponse {
        schema_version: SCHEMA_VERSION,
        workspace_root: config.workspace_root.to_string_lossy().to_string(),
        build_system: config.build_system.into(),
        java: JavaConfigEntry {
            source: config.java.source.0,
            target: config.java.target.0,
        },
        modules: config
            .modules
            .into_iter()
            .map(|m| ModuleEntry {
                name: m.name,
                root: m.root.to_string_lossy().to_string(),
            })
            .collect(),
        source_roots: config
            .source_roots
            .into_iter()
            .map(|root| SourceRootEntry {
                kind: root.kind.into(),
                origin: root.origin.into(),
                path: root.path.to_string_lossy().to_string(),
            })
            .collect(),
        classpath: config
            .classpath
            .into_iter()
            .map(|entry| ClasspathEntry {
                kind: entry.kind.into(),
                path: entry.path.to_string_lossy().to_string(),
            })
            .collect(),
        module_path: config
            .module_path
            .into_iter()
            .map(|entry| ClasspathEntry {
                kind: entry.kind.into(),
                path: entry.path.to_string_lossy().to_string(),
            })
            .collect(),
        output_dirs: config
            .output_dirs
            .into_iter()
            .map(|dir| OutputDirEntry {
                kind: dir.kind.into(),
                path: dir.path.to_string_lossy().to_string(),
            })
            .collect(),
        dependencies: config
            .dependencies
            .into_iter()
            .map(|dep| DependencyEntry {
                group_id: dep.group_id,
                artifact_id: dep.artifact_id,
                version: dep.version,
                scope: dep.scope,
                classifier: dep.classifier,
                type_: dep.type_,
            })
            .collect(),
    };

    serde_json::to_value(resp)
        .map_err(|err| NovaLspError::Internal(crate::sanitize_serde_json_error(&err)))
}
