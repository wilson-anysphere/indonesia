//! JPMS compilation environment builder.
//!
//! This module assembles a single [`nova_modules::ModuleGraph`] from:
//! - the JDK module set
//! - workspace modules (`module-info.java`)
//! - module-path entries (explicit or automatic modules)
//! - a sentinel unnamed module representing the classpath

use std::collections::HashSet;

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use nova_classpath::ClasspathEntry;
use nova_modules::{ModuleGraph, ModuleInfo, ModuleKind, ModuleName, JAVA_BASE};

#[derive(Debug, Clone)]
pub struct JpmsEnvironment {
    pub graph: ModuleGraph,
    pub unnamed: ModuleName,
}

pub fn build_jpms_environment(
    jdk: &nova_jdk::JdkIndex,
    workspace: Option<&nova_project::ProjectConfig>,
    module_path_entries: &[ClasspathEntry],
) -> Result<JpmsEnvironment> {
    let mut graph = jdk.module_graph().cloned().unwrap_or_else(|| {
        let mut graph = ModuleGraph::new();
        graph.insert(empty_module(
            ModuleKind::Explicit,
            ModuleName::new(JAVA_BASE),
        ));
        graph
    });

    let mut workspace_modules: HashSet<ModuleName> = HashSet::new();
    if let Some(workspace) = workspace {
        for root in &workspace.jpms_modules {
            let info = root.info.clone();

            if graph.get(&info.name).is_some() {
                return Err(anyhow!(
                    "workspace module `{}` collides with an existing module of the same name",
                    info.name
                ));
            }

            workspace_modules.insert(info.name.clone());
            graph.insert(info);
        }
    }

    for entry in module_path_entries {
        let info = match entry.module_info().with_context(|| {
            format!(
                "failed to read module-info for `{}`",
                entry.path().display()
            )
        })? {
            Some(info) => info,
            None => {
                let meta = entry.module_meta().with_context(|| {
                    format!(
                        "failed to determine JPMS module name for `{}`",
                        entry.path().display()
                    )
                })?;
                let name = meta
                    .name
                    .or_else(|| derive_automatic_module_name(entry.path()).map(ModuleName::new))
                    .ok_or_else(|| {
                        anyhow!(
                            "failed to determine JPMS module name for `{}`",
                            entry.path().display()
                        )
                    })?;
                empty_module(ModuleKind::Automatic, name)
            }
        };

        if workspace_modules.contains(&info.name) {
            // Prefer workspace module descriptors over external modules on the module-path.
            continue;
        }

        if graph.get(&info.name).is_some() {
            return Err(anyhow!(
                "module-path entry `{}` defines module `{}` which collides with an existing module",
                entry.path().display(),
                info.name
            ));
        }

        graph.insert(info);
    }

    let unnamed = ModuleName::unnamed();
    if graph.get(&unnamed).is_none() {
        graph.insert(ModuleInfo {
            name: unnamed.clone(),
            kind: ModuleKind::Unnamed,
            is_open: true,
            requires: Vec::new(),
            exports: Vec::new(),
            opens: Vec::new(),
            uses: Vec::new(),
            provides: Vec::new(),
        });
    }

    Ok(JpmsEnvironment { graph, unnamed })
}

fn empty_module(kind: ModuleKind, name: ModuleName) -> ModuleInfo {
    ModuleInfo {
        name,
        kind,
        is_open: false,
        requires: Vec::new(),
        exports: Vec::new(),
        opens: Vec::new(),
        uses: Vec::new(),
        provides: Vec::new(),
    }
}

fn derive_automatic_module_name(path: &Path) -> Option<String> {
    let stem = if path.is_dir() {
        path.file_name()?.to_string_lossy()
    } else {
        path.file_stem()?.to_string_lossy()
    };
    derive_automatic_module_name_from_stem(&stem)
}

// Keep this logic in sync with the algorithm used by the JDK for automatic
// modules (and mirrored by `nova-classpath`). This is duplicated here because
// `nova-classpath` keeps the helper internal for now.
fn derive_automatic_module_name_from_stem(stem: &str) -> Option<String> {
    if stem.is_empty() {
        return None;
    }

    let stem = strip_version_suffix(stem);

    let mut normalized = String::with_capacity(stem.len());
    let mut last_was_dot = true;
    for ch in stem.chars() {
        if ch.is_ascii_alphanumeric() {
            normalized.push(ch);
            last_was_dot = false;
        } else if !last_was_dot {
            normalized.push('.');
            last_was_dot = true;
        }
    }
    if normalized.ends_with('.') {
        normalized.pop();
    }

    if normalized.is_empty() {
        return Some("_".to_string());
    }

    let mut parts_out = Vec::new();
    for part in normalized.split('.') {
        if part.is_empty() {
            continue;
        }
        if part.as_bytes().first().is_some_and(|b| b.is_ascii_digit()) {
            parts_out.push(format!("_{part}"));
        } else {
            parts_out.push(part.to_string());
        }
    }

    if parts_out.is_empty() {
        Some("_".to_string())
    } else {
        Some(parts_out.join("."))
    }
}

fn strip_version_suffix(stem: &str) -> &str {
    let bytes = stem.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'-' && bytes[i + 1].is_ascii_digit() {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j == bytes.len() || bytes[j] == b'.' {
                return &stem[..i];
            }
        }
        i += 1;
    }
    stem
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    use nova_hir::module_info::lower_module_info_source_strict;
    use nova_jdk::JdkIndex;
    use nova_project::{BuildSystem, JavaConfig, ProjectConfig};
    use nova_project::{JpmsModuleRoot, Module};
    use tempfile::TempDir;

    fn test_dep_jar() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../nova-classpath/testdata/dep.jar")
    }

    fn test_named_module_jar() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../nova-classpath/testdata/named-module.jar")
    }

    #[test]
    fn builds_environment_from_jdk_workspace_and_module_path() {
        let tmp = TempDir::new().unwrap();

        let mod_a = tmp.path().join("mod-a");
        let mod_b = tmp.path().join("mod-b");
        let mod_c = tmp.path().join("mod-c");
        std::fs::create_dir_all(&mod_a).unwrap();
        std::fs::create_dir_all(&mod_b).unwrap();
        std::fs::create_dir_all(&mod_c).unwrap();

        let src_a = "module workspace.a { requires workspace.b; }";
        let src_b = "module workspace.b { }";
        let src_c = "module workspace.c { requires dep; }";

        std::fs::write(mod_a.join("module-info.java"), src_a).unwrap();
        std::fs::write(mod_b.join("module-info.java"), src_b).unwrap();
        std::fs::write(mod_c.join("module-info.java"), src_c).unwrap();

        let info_a = lower_module_info_source_strict(src_a).unwrap();
        let info_b = lower_module_info_source_strict(src_b).unwrap();
        let info_c = lower_module_info_source_strict(src_c).unwrap();

        let ws = ProjectConfig {
            workspace_root: tmp.path().to_path_buf(),
            build_system: BuildSystem::Simple,
            java: JavaConfig::default(),
            modules: vec![Module {
                name: "dummy".to_string(),
                root: tmp.path().to_path_buf(),
            }],
            jpms_modules: vec![
                JpmsModuleRoot {
                    name: ModuleName::new("workspace.a"),
                    root: mod_a.clone(),
                    module_info: mod_a.join("module-info.java"),
                    info: info_a,
                },
                JpmsModuleRoot {
                    name: ModuleName::new("workspace.b"),
                    root: mod_b.clone(),
                    module_info: mod_b.join("module-info.java"),
                    info: info_b,
                },
                JpmsModuleRoot {
                    name: ModuleName::new("workspace.c"),
                    root: mod_c.clone(),
                    module_info: mod_c.join("module-info.java"),
                    info: info_c,
                },
            ],
            source_roots: Vec::new(),
            module_path: Vec::new(),
            classpath: Vec::new(),
            output_dirs: Vec::new(),
            dependencies: Vec::new(),
        };

        let extra_dir = tmp.path().join("extra-dep-1.2.3");
        std::fs::create_dir_all(&extra_dir).unwrap();

        let jdk = JdkIndex::new();
        let env = build_jpms_environment(
            &jdk,
            Some(&ws),
            &[
                ClasspathEntry::Jar(test_dep_jar()),
                ClasspathEntry::ClassDir(extra_dir),
            ],
        )
        .unwrap();

        let java_base = ModuleName::new("java.base");
        assert!(env.graph.get(&java_base).is_some());

        let a = ModuleName::new("workspace.a");
        let b = ModuleName::new("workspace.b");
        let c = ModuleName::new("workspace.c");
        let dep = ModuleName::new("dep");

        assert!(env.graph.can_read(&a, &b));
        assert!(!env.graph.can_read(&b, &a));

        // Automatic modules read all named modules.
        assert!(env.graph.can_read(&dep, &a));

        // Requiring an automatic module makes everything readable (best-effort JPMS semantics).
        assert!(env.graph.can_read(&c, &b));

        // Directory module-path entries without module descriptors become automatic modules.
        let extra = ModuleName::new("extra.dep");
        assert!(env
            .graph
            .get(&extra)
            .is_some_and(|info| info.kind == ModuleKind::Automatic));

        // The unnamed module reads everything; named modules do not read it.
        assert!(env.graph.can_read(&env.unnamed, &a));
        assert!(!env.graph.can_read(&a, &env.unnamed));
    }

    #[test]
    fn workspace_modules_shadow_module_path_modules() {
        let tmp = TempDir::new().unwrap();

        let mod_root = tmp.path().join("example-mod");
        std::fs::create_dir_all(&mod_root).unwrap();

        let src = "module example.mod { exports workspace.pkg; }";
        std::fs::write(mod_root.join("module-info.java"), src).unwrap();
        let info = lower_module_info_source_strict(src).unwrap();

        let ws = ProjectConfig {
            workspace_root: tmp.path().to_path_buf(),
            build_system: BuildSystem::Simple,
            java: JavaConfig::default(),
            modules: vec![Module {
                name: "dummy".to_string(),
                root: tmp.path().to_path_buf(),
            }],
            jpms_modules: vec![JpmsModuleRoot {
                name: ModuleName::new("example.mod"),
                root: mod_root.clone(),
                module_info: mod_root.join("module-info.java"),
                info,
            }],
            source_roots: Vec::new(),
            module_path: Vec::new(),
            classpath: Vec::new(),
            output_dirs: Vec::new(),
            dependencies: Vec::new(),
        };

        let jdk = JdkIndex::new();
        let env = build_jpms_environment(
            &jdk,
            Some(&ws),
            &[ClasspathEntry::Jar(test_named_module_jar())],
        )
        .unwrap();

        let module = ModuleName::new("example.mod");
        let info = env.graph.get(&module).expect("workspace module should exist");
        assert_eq!(info.kind, ModuleKind::Explicit);
        assert!(info.exports.iter().any(|e| e.package == "workspace.pkg"));
    }
}
