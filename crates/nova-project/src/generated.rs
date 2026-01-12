use std::collections::HashSet;
use std::path::{Path, PathBuf};

use nova_build_model::{GeneratedRootsSnapshotFile, GENERATED_ROOTS_SNAPSHOT_SCHEMA_VERSION};
use nova_config::NovaConfig;

use crate::{BuildSystem, SourceRoot, SourceRootKind, SourceRootOrigin};

pub(crate) fn append_generated_source_roots(
    roots: &mut Vec<SourceRoot>,
    workspace_root: &Path,
    module_root: &Path,
    build_system: BuildSystem,
    config: &NovaConfig,
) {
    if !config.generated_sources.enabled {
        return;
    }

    let mut candidates: Vec<(SourceRootKind, PathBuf)> = Vec::new();

    if let Some(override_roots) = &config.generated_sources.override_roots {
        for root in override_roots {
            let path = if root.is_absolute() {
                root.clone()
            } else {
                module_root.join(root)
            };
            candidates.push((SourceRootKind::Main, path));
        }
    } else {
        candidates.extend(read_snapshot_roots(workspace_root, module_root));

        match build_system {
            BuildSystem::Maven => {
                candidates.push((
                    SourceRootKind::Main,
                    module_root.join("target/generated-sources"),
                ));
                candidates.push((
                    SourceRootKind::Main,
                    module_root.join("target/generated-sources/annotations"),
                ));
                candidates.push((
                    SourceRootKind::Test,
                    module_root.join("target/generated-test-sources"),
                ));
                candidates.push((
                    SourceRootKind::Test,
                    module_root.join("target/generated-test-sources/test-annotations"),
                ));
            }
            BuildSystem::Gradle => {
                candidates.push((
                    SourceRootKind::Main,
                    module_root.join("build/generated/sources/annotationProcessor/java/main"),
                ));
                candidates.push((
                    SourceRootKind::Test,
                    module_root.join("build/generated/sources/annotationProcessor/java/test"),
                ));
            }
            BuildSystem::Simple => {
                // Heuristic: include both Maven and Gradle conventions.
                candidates.push((
                    SourceRootKind::Main,
                    module_root.join("target/generated-sources/annotations"),
                ));
                candidates.push((
                    SourceRootKind::Test,
                    module_root.join("target/generated-test-sources/test-annotations"),
                ));
                candidates.push((
                    SourceRootKind::Main,
                    module_root.join("build/generated/sources/annotationProcessor/java/main"),
                ));
                candidates.push((
                    SourceRootKind::Test,
                    module_root.join("build/generated/sources/annotationProcessor/java/test"),
                ));
            }
            BuildSystem::Bazel => {}
        }

        for root in &config.generated_sources.additional_roots {
            let path = if root.is_absolute() {
                root.clone()
            } else {
                module_root.join(root)
            };
            candidates.push((SourceRootKind::Main, path));
        }
    }

    let mut seen = HashSet::new();
    for (kind, path) in candidates {
        if !seen.insert(path.clone()) {
            continue;
        }

        roots.push(SourceRoot {
            kind,
            origin: SourceRootOrigin::Generated,
            path,
        });
    }
}

fn read_snapshot_roots(
    workspace_root: &Path,
    module_root: &Path,
) -> Vec<(SourceRootKind, PathBuf)> {
    let snapshot_path = workspace_root
        .join(".nova")
        .join("apt-cache")
        .join("generated-roots.json");
    let text = match std::fs::read_to_string(snapshot_path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(_) => return Vec::new(),
    };

    let file: GeneratedRootsSnapshotFile = match serde_json::from_str(&text) {
        Ok(file) => file,
        Err(_) => return Vec::new(),
    };

    if file.schema_version != GENERATED_ROOTS_SNAPSHOT_SCHEMA_VERSION {
        return Vec::new();
    }

    for module in file.modules {
        let module_root_candidate = if module.module_root.is_absolute() {
            module.module_root
        } else {
            workspace_root.join(module.module_root)
        };
        if module_root_candidate != module_root {
            continue;
        }

        return module
            .roots
            .into_iter()
            .map(|root| {
                let kind: SourceRootKind = root.kind.into();
                let path = if root.path.is_absolute() {
                    root.path
                } else {
                    module_root_candidate.join(root.path)
                };
                (kind, path)
            })
            .collect();
    }

    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn snapshot_generated_roots_are_appended() {
        let temp = TempDir::new().expect("tempdir");
        let workspace_root = temp.path();
        let module_root = workspace_root.join("module");
        std::fs::create_dir_all(&module_root).expect("create module");

        let snapshot_dir = workspace_root.join(".nova").join("apt-cache");
        std::fs::create_dir_all(&snapshot_dir).expect("create snapshot dir");

        let custom_root = module_root.join("custom-generated");
        let snapshot = serde_json::json!({
            "schema_version": GENERATED_ROOTS_SNAPSHOT_SCHEMA_VERSION,
            "modules": [{
                "module_root": module_root.to_string_lossy(),
                "roots": [{
                    "kind": "main",
                    "path": custom_root.to_string_lossy(),
                }]
            }]
        });
        std::fs::write(
            snapshot_dir.join("generated-roots.json"),
            serde_json::to_string_pretty(&snapshot).expect("serialize snapshot"),
        )
        .expect("write snapshot");

        let config = NovaConfig::default();
        let mut roots = Vec::new();
        append_generated_source_roots(
            &mut roots,
            workspace_root,
            &module_root,
            BuildSystem::Simple,
            &config,
        );

        assert!(
            roots.iter().any(|root| root.path == custom_root),
            "expected custom root to be appended; got: {roots:?}"
        );
    }

    #[test]
    fn maven_default_generated_roots_are_appended() {
        let temp = TempDir::new().expect("tempdir");
        let workspace_root = temp.path();
        let module_root = workspace_root.join("module");
        std::fs::create_dir_all(&module_root).expect("create module");

        let config = NovaConfig::default();
        let mut roots = Vec::new();
        append_generated_source_roots(
            &mut roots,
            workspace_root,
            &module_root,
            BuildSystem::Maven,
            &config,
        );

        let expected = [
            (
                SourceRootKind::Main,
                module_root.join("target/generated-sources"),
            ),
            (
                SourceRootKind::Main,
                module_root.join("target/generated-sources/annotations"),
            ),
            (
                SourceRootKind::Test,
                module_root.join("target/generated-test-sources"),
            ),
            (
                SourceRootKind::Test,
                module_root.join("target/generated-test-sources/test-annotations"),
            ),
        ];

        for (kind, path) in expected {
            assert!(
                roots
                    .iter()
                    .any(|root| root.kind == kind && root.path == path),
                "expected {kind:?} root {path:?} to be appended; got: {roots:?}"
            );
        }
    }
}
