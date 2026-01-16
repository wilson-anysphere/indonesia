use crate::{NovaLspError, Result};
use serde_json::Value;
use std::path::PathBuf;

pub const SCHEMA_VERSION: u32 = 1;

fn build_system_string(kind: nova_project::BuildSystem) -> &'static str {
    match kind {
        nova_project::BuildSystem::Maven => "maven",
        nova_project::BuildSystem::Gradle => "gradle",
        nova_project::BuildSystem::Bazel => "bazel",
        nova_project::BuildSystem::Simple => "simple",
    }
}

pub(crate) fn source_root_kind_string(kind: nova_project::SourceRootKind) -> &'static str {
    match kind {
        nova_project::SourceRootKind::Main => "main",
        nova_project::SourceRootKind::Test => "test",
    }
}

pub(crate) fn source_root_origin_string(origin: nova_project::SourceRootOrigin) -> &'static str {
    match origin {
        nova_project::SourceRootOrigin::Source => "source",
        nova_project::SourceRootOrigin::Generated => "generated",
    }
}

fn classpath_entry_kind_string(kind: nova_project::ClasspathEntryKind) -> &'static str {
    match kind {
        nova_project::ClasspathEntryKind::Directory => "directory",
        nova_project::ClasspathEntryKind::Jar => "jar",
    }
}

fn output_dir_kind_string(kind: nova_project::OutputDirKind) -> &'static str {
    match kind {
        nova_project::OutputDirKind::Main => "main",
        nova_project::OutputDirKind::Test => "test",
    }
}

pub fn handle_project_configuration(params: serde_json::Value) -> Result<serde_json::Value> {
    let project_root = super::decode_project_root(params)?;
    if project_root.trim().is_empty() {
        return Err(NovaLspError::InvalidParams(
            "`projectRoot` must not be empty".to_string(),
        ));
    }

    let root = PathBuf::from(&project_root);
    let config = nova_project::load_project(&root)
        .map_err(|err| NovaLspError::Internal(format!("failed to load project: {err}")))?;

    Ok(Value::Object({
        let mut resp = serde_json::Map::new();
        resp.insert(
            "schemaVersion".to_string(),
            Value::from(u64::from(SCHEMA_VERSION)),
        );
        resp.insert(
            "workspaceRoot".to_string(),
            Value::String(config.workspace_root.to_string_lossy().to_string()),
        );
        resp.insert(
            "buildSystem".to_string(),
            Value::String(build_system_string(config.build_system).to_string()),
        );
        resp.insert(
            "java".to_string(),
            Value::Object({
                let mut java = serde_json::Map::new();
                java.insert(
                    "source".to_string(),
                    Value::from(config.java.source.0 as u64),
                );
                java.insert(
                    "target".to_string(),
                    Value::from(config.java.target.0 as u64),
                );
                java
            }),
        );
        resp.insert(
            "modules".to_string(),
            Value::Array(
                config
                    .modules
                    .into_iter()
                    .map(|m| {
                        Value::Object({
                            let mut module = serde_json::Map::new();
                            module.insert("name".to_string(), Value::String(m.name));
                            module.insert(
                                "root".to_string(),
                                Value::String(m.root.to_string_lossy().to_string()),
                            );
                            module
                        })
                    })
                    .collect(),
            ),
        );
        resp.insert(
            "sourceRoots".to_string(),
            Value::Array(
                config
                    .source_roots
                    .into_iter()
                    .map(|root| {
                        Value::Object({
                            let mut entry = serde_json::Map::new();
                            entry.insert(
                                "kind".to_string(),
                                Value::String(source_root_kind_string(root.kind).to_string()),
                            );
                            entry.insert(
                                "origin".to_string(),
                                Value::String(source_root_origin_string(root.origin).to_string()),
                            );
                            entry.insert(
                                "path".to_string(),
                                Value::String(root.path.to_string_lossy().to_string()),
                            );
                            entry
                        })
                    })
                    .collect(),
            ),
        );
        resp.insert(
            "classpath".to_string(),
            Value::Array(
                config
                    .classpath
                    .into_iter()
                    .map(|entry| {
                        Value::Object({
                            let mut value = serde_json::Map::new();
                            value.insert(
                                "kind".to_string(),
                                Value::String(classpath_entry_kind_string(entry.kind).to_string()),
                            );
                            value.insert(
                                "path".to_string(),
                                Value::String(entry.path.to_string_lossy().to_string()),
                            );
                            value
                        })
                    })
                    .collect(),
            ),
        );
        resp.insert(
            "modulePath".to_string(),
            Value::Array(
                config
                    .module_path
                    .into_iter()
                    .map(|entry| {
                        Value::Object({
                            let mut value = serde_json::Map::new();
                            value.insert(
                                "kind".to_string(),
                                Value::String(classpath_entry_kind_string(entry.kind).to_string()),
                            );
                            value.insert(
                                "path".to_string(),
                                Value::String(entry.path.to_string_lossy().to_string()),
                            );
                            value
                        })
                    })
                    .collect(),
            ),
        );
        resp.insert(
            "outputDirs".to_string(),
            Value::Array(
                config
                    .output_dirs
                    .into_iter()
                    .map(|dir| {
                        Value::Object({
                            let mut value = serde_json::Map::new();
                            value.insert(
                                "kind".to_string(),
                                Value::String(output_dir_kind_string(dir.kind).to_string()),
                            );
                            value.insert(
                                "path".to_string(),
                                Value::String(dir.path.to_string_lossy().to_string()),
                            );
                            value
                        })
                    })
                    .collect(),
            ),
        );
        resp.insert(
            "dependencies".to_string(),
            Value::Array(
                config
                    .dependencies
                    .into_iter()
                    .map(|dep| {
                        Value::Object({
                            let mut value = serde_json::Map::new();
                            value.insert("groupId".to_string(), Value::String(dep.group_id));
                            value.insert("artifactId".to_string(), Value::String(dep.artifact_id));
                            if let Some(version) = dep.version {
                                value.insert("version".to_string(), Value::String(version));
                            }
                            if let Some(scope) = dep.scope {
                                value.insert("scope".to_string(), Value::String(scope));
                            }
                            if let Some(classifier) = dep.classifier {
                                value.insert("classifier".to_string(), Value::String(classifier));
                            }
                            if let Some(type_) = dep.type_ {
                                value.insert("type".to_string(), Value::String(type_));
                            }
                            value
                        })
                    })
                    .collect(),
            ),
        );
        resp
    }))
}
