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
use nova_classpath::{ClasspathEntry, IndexOptions};
use nova_modules::{ModuleGraph, ModuleInfo, ModuleKind, ModuleName, JAVA_BASE};

#[derive(Debug, Clone)]
pub struct JpmsEnvironment {
    pub graph: ModuleGraph,
    pub unnamed: ModuleName,
}

pub struct JpmsCompilationEnvironment {
    pub env: JpmsEnvironment,
    pub classpath: nova_classpath::ModuleAwareClasspathIndex,
}

impl std::fmt::Debug for JpmsCompilationEnvironment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JpmsCompilationEnvironment")
            .field("env", &self.env)
            .field("classpath", &"<module-aware classpath index>")
            .finish()
    }
}

impl JpmsCompilationEnvironment {
    /// Approximate heap memory usage of this compilation environment in bytes.
    ///
    /// This is intended for best-effort integration with `nova-memory`.
    pub fn estimated_bytes(&self) -> u64 {
        let mut bytes = 0u64;
        bytes = bytes.saturating_add(self.env.graph.estimated_bytes());
        // The unnamed module name is also present in the graph, but keep this simple and
        // count the additional allocation as well.
        bytes = bytes.saturating_add(self.env.unnamed.as_str().len() as u64);
        bytes = bytes.saturating_add(self.classpath.estimated_bytes());
        bytes
    }
}

pub fn build_jpms_environment(
    jdk: &nova_jdk::JdkIndex,
    workspace: Option<&nova_build_model::ProjectConfig>,
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
                let meta = entry.module_meta_for_module_path().with_context(|| {
                    format!(
                        "failed to determine JPMS module name for `{}`",
                        entry.path().display()
                    )
                })?;
                let name = meta.name.ok_or_else(|| {
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

pub fn build_jpms_environment_for_project(
    jdk: &nova_jdk::JdkIndex,
    project: &nova_build_model::ProjectConfig,
) -> Result<JpmsEnvironment> {
    let module_path_entries: Vec<ClasspathEntry> = project
        .module_path
        .iter()
        .map(ClasspathEntry::from)
        .collect();

    build_jpms_environment(jdk, Some(project), &module_path_entries)
}

pub fn build_jpms_compilation_environment(
    jdk: &nova_jdk::JdkIndex,
    workspace: Option<&nova_build_model::ProjectConfig>,
    module_path_entries: &[ClasspathEntry],
    classpath_entries: &[ClasspathEntry],
    cache_dir: Option<&Path>,
) -> Result<JpmsCompilationEnvironment> {
    let target_release = workspace
        .map(|workspace| workspace.java.target.0)
        .filter(|release| *release >= 1)
        .or(jdk.info().api_release);
    let options = IndexOptions { target_release };
    build_jpms_compilation_environment_with_options(
        jdk,
        workspace,
        module_path_entries,
        classpath_entries,
        cache_dir,
        options,
    )
}

pub fn build_jpms_compilation_environment_with_options(
    jdk: &nova_jdk::JdkIndex,
    workspace: Option<&nova_build_model::ProjectConfig>,
    module_path_entries: &[ClasspathEntry],
    classpath_entries: &[ClasspathEntry],
    cache_dir: Option<&Path>,
    options: IndexOptions,
) -> Result<JpmsCompilationEnvironment> {
    let mut env = build_jpms_environment(jdk, workspace, module_path_entries)?;

    // Some build tools keep non-modular JARs on the classpath even for JPMS compilation. In
    // practice they often apply `--add-reads <module>=ALL-UNNAMED` so that workspace modules can
    // still access types from the classpath's unnamed module.
    //
    // Nova's default JPMS model is strict (named modules do not read the unnamed module). When
    // building a compilation environment with classpath entries, we model the common
    // `--add-reads <module>=ALL-UNNAMED` behavior by making every workspace module read the unnamed
    // module.
    if let Some(workspace) = workspace {
        if !classpath_entries.is_empty() {
            for root in &workspace.jpms_modules {
                let Some(mut info) = env.graph.get(&root.name).cloned() else {
                    continue;
                };

                if !info.requires.iter().any(|req| req.module.is_unnamed()) {
                    info.requires.push(nova_modules::Requires {
                        module: ModuleName::unnamed(),
                        is_transitive: false,
                        is_static: false,
                    });
                    env.graph.insert(info);
                }
            }
        }
    }

    let classpath = nova_classpath::ModuleAwareClasspathIndex::build_mixed_with_options(
        module_path_entries,
        classpath_entries,
        cache_dir,
        options,
    )
    .context("failed to index classpath/module-path entries")?;

    Ok(JpmsCompilationEnvironment { env, classpath })
}

pub fn build_jpms_compilation_environment_for_project(
    jdk: &nova_jdk::JdkIndex,
    project: &nova_build_model::ProjectConfig,
    cache_dir: Option<&Path>,
) -> Result<JpmsCompilationEnvironment> {
    let module_path_entries: Vec<ClasspathEntry> = project
        .module_path
        .iter()
        .map(ClasspathEntry::from)
        .collect();
    let classpath_entries: Vec<ClasspathEntry> =
        project.classpath.iter().map(ClasspathEntry::from).collect();

    build_jpms_compilation_environment(
        jdk,
        Some(project),
        &module_path_entries,
        &classpath_entries,
        cache_dir,
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::Write;
    use std::path::PathBuf;

    use crate::jpms::JpmsResolver;
    use nova_build_model::{
        BuildSystem, JavaConfig, JavaVersion, JpmsModuleRoot, Module, ProjectConfig,
    };
    use nova_classpath::ModuleNameKind;
    use nova_core::{QualifiedName, TypeName};
    use nova_hir::module_info::lower_module_info_source_strict;
    use nova_jdk::JdkIndex;
    use tempfile::TempDir;

    fn test_dep_jar() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../nova-classpath/testdata/dep.jar")
    }

    fn test_named_module_jar() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../nova-classpath/testdata/named-module.jar")
    }

    #[test]
    fn accepts_missing_module_path_archives() {
        let tmp = TempDir::new().unwrap();
        let jdk = JdkIndex::new();

        // Do not create these archives on disk: build loaders can synthesize jar/jmod paths before
        // downloading them. We should still be able to build a JPMS environment by deriving
        // automatic module names from the filenames.
        let missing_jar = tmp.path().join("missing-dep-1.2.3.jar");
        let missing_jmod = tmp.path().join("missing-jmod.JMOD");

        let module_path = vec![
            ClasspathEntry::Jar(missing_jar.clone()),
            ClasspathEntry::Jmod(missing_jmod.clone()),
        ];

        let env = build_jpms_environment(&jdk, None, &module_path).expect("build env");
        let jar_mod = ModuleName::new("missing.dep");
        let jmod_mod = ModuleName::new("missing.jmod");
        assert!(env
            .graph
            .get(&jar_mod)
            .is_some_and(|m| m.kind == ModuleKind::Automatic));
        assert!(env
            .graph
            .get(&jmod_mod)
            .is_some_and(|m| m.kind == ModuleKind::Automatic));

        // The compilation environment should also build successfully (classpath indexing should
        // not fail on missing archives).
        build_jpms_compilation_environment(&jdk, None, &module_path, &[], None)
            .expect("build compilation env");
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
                annotation_processing: Default::default(),
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
            jpms_workspace: None,
            source_roots: Vec::new(),
            module_path: Vec::new(),
            classpath: Vec::new(),
            output_dirs: Vec::new(),
            dependencies: Vec::new(),
            workspace_model: None,
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
                annotation_processing: Default::default(),
            }],
            jpms_modules: vec![JpmsModuleRoot {
                name: ModuleName::new("example.mod"),
                root: mod_root.clone(),
                module_info: mod_root.join("module-info.java"),
                info,
            }],
            jpms_workspace: None,
            source_roots: Vec::new(),
            module_path: Vec::new(),
            classpath: Vec::new(),
            output_dirs: Vec::new(),
            dependencies: Vec::new(),
            workspace_model: None,
        };

        let jdk = JdkIndex::new();
        let env = build_jpms_environment(
            &jdk,
            Some(&ws),
            &[ClasspathEntry::Jar(test_named_module_jar())],
        )
        .unwrap();

        let module = ModuleName::new("example.mod");
        let info = env
            .graph
            .get(&module)
            .expect("workspace module should exist");
        assert_eq!(info.kind, ModuleKind::Explicit);
        assert!(info.exports.iter().any(|e| e.package == "workspace.pkg"));
    }

    #[test]
    fn compilation_environment_treats_classpath_jars_as_unnamed() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let ws = ProjectConfig {
            workspace_root: root.clone(),
            build_system: BuildSystem::Simple,
            java: JavaConfig::default(),
            modules: vec![Module {
                name: "dummy".to_string(),
                root: root.clone(),
                annotation_processing: Default::default(),
            }],
            jpms_modules: Vec::new(),
            jpms_workspace: None,
            source_roots: Vec::new(),
            module_path: Vec::new(),
            classpath: Vec::new(),
            output_dirs: Vec::new(),
            dependencies: Vec::new(),
            workspace_model: None,
        };

        let module_path = [ClasspathEntry::Jar(test_named_module_jar())];
        let classpath = [ClasspathEntry::Jar(test_dep_jar())];

        let jdk = JdkIndex::new();
        let env =
            build_jpms_compilation_environment(&jdk, Some(&ws), &module_path, &classpath, None)
                .unwrap();

        // Module-path jar should have module metadata.
        let module = env
            .classpath
            .module_of("com.example.api.Api")
            .expect("expected module-path module metadata");
        assert_eq!(module.as_str(), "example.mod");
        assert_eq!(
            env.classpath.module_kind_of("com.example.api.Api"),
            ModuleNameKind::Explicit
        );

        // Classpath jar should be treated as unnamed (no module metadata).
        assert!(env.classpath.module_of("com.example.dep.Foo").is_none());
        assert_eq!(
            env.classpath.module_kind_of("com.example.dep.Foo"),
            ModuleNameKind::None
        );
    }

    #[test]
    fn jpms_modules_can_read_all_unnamed_when_classpath_present() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        let mod_a = tmp.path().join("mod-a");
        std::fs::create_dir_all(&mod_a).unwrap();
        let src_a = "module workspace.a { }";
        std::fs::write(mod_a.join("module-info.java"), src_a).unwrap();
        let info_a = lower_module_info_source_strict(src_a).unwrap();

        let ws = ProjectConfig {
            workspace_root: root.clone(),
            build_system: BuildSystem::Simple,
            java: JavaConfig::default(),
            modules: vec![Module {
                name: "dummy".to_string(),
                root,
                annotation_processing: Default::default(),
            }],
            jpms_modules: vec![JpmsModuleRoot {
                name: ModuleName::new("workspace.a"),
                root: mod_a.clone(),
                module_info: mod_a.join("module-info.java"),
                info: info_a,
            }],
            jpms_workspace: None,
            source_roots: Vec::new(),
            module_path: Vec::new(),
            classpath: Vec::new(),
            output_dirs: Vec::new(),
            dependencies: Vec::new(),
            workspace_model: None,
        };

        let module_path: [ClasspathEntry; 0] = [];
        let classpath = [ClasspathEntry::Jar(test_dep_jar())];

        let jdk = JdkIndex::new();
        let env =
            build_jpms_compilation_environment(&jdk, Some(&ws), &module_path, &classpath, None)
                .unwrap();

        let from_a = ModuleName::new("workspace.a");
        assert!(env.env.graph.can_read(&from_a, &env.env.unnamed));

        let ty = QualifiedName::from_dotted("com.example.dep.Foo");
        let resolver_a = JpmsResolver::new(&jdk, &env.env.graph, &env.classpath, from_a);
        assert_eq!(
            resolver_a.resolve_qualified_name(&ty),
            Some(TypeName::from("com.example.dep.Foo"))
        );
    }

    #[test]
    fn jpms_resolver_still_enforces_requires_for_module_path_types_with_classpath_present() {
        let tmp = TempDir::new().unwrap();

        let mod_a = tmp.path().join("mod-a");
        let mod_b = tmp.path().join("mod-b");
        std::fs::create_dir_all(&mod_a).unwrap();
        std::fs::create_dir_all(&mod_b).unwrap();

        let src_a = "module workspace.a { }";
        let src_b = "module workspace.b { requires dep; }";

        std::fs::write(mod_a.join("module-info.java"), src_a).unwrap();
        std::fs::write(mod_b.join("module-info.java"), src_b).unwrap();

        let info_a = lower_module_info_source_strict(src_a).unwrap();
        let info_b = lower_module_info_source_strict(src_b).unwrap();

        let ws = ProjectConfig {
            workspace_root: tmp.path().to_path_buf(),
            build_system: BuildSystem::Simple,
            java: JavaConfig::default(),
            modules: vec![Module {
                name: "dummy".to_string(),
                root: tmp.path().to_path_buf(),
                annotation_processing: Default::default(),
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
            ],
            jpms_workspace: None,
            source_roots: Vec::new(),
            module_path: Vec::new(),
            classpath: Vec::new(),
            output_dirs: Vec::new(),
            dependencies: Vec::new(),
            workspace_model: None,
        };

        let module_path = [ClasspathEntry::Jar(test_dep_jar())];
        let classpath = [ClasspathEntry::Jar(test_named_module_jar())];

        let jdk = JdkIndex::new();
        let env =
            build_jpms_compilation_environment(&jdk, Some(&ws), &module_path, &classpath, None)
                .unwrap();

        let ty = QualifiedName::from_dotted("com.example.dep.Foo");

        let from_a = ModuleName::new("workspace.a");
        assert!(
            env.env.graph.can_read(&from_a, &env.env.unnamed),
            "workspace.a should read the unnamed module when classpath entries are present"
        );
        let resolver_a = JpmsResolver::new(&jdk, &env.env.graph, &env.classpath, from_a);
        assert_eq!(resolver_a.resolve_qualified_name(&ty), None);

        let from_b = ModuleName::new("workspace.b");
        assert!(
            env.env.graph.can_read(&from_b, &env.env.unnamed),
            "workspace.b should read the unnamed module when classpath entries are present"
        );
        let resolver_b = JpmsResolver::new(&jdk, &env.env.graph, &env.classpath, from_b);
        assert_eq!(
            resolver_b.resolve_qualified_name(&ty),
            Some(TypeName::from("com.example.dep.Foo"))
        );
    }

    #[test]
    fn jpms_resolver_enforces_requires_for_module_path_types() {
        let tmp = TempDir::new().unwrap();

        let mod_a = tmp.path().join("mod-a");
        let mod_b = tmp.path().join("mod-b");
        std::fs::create_dir_all(&mod_a).unwrap();
        std::fs::create_dir_all(&mod_b).unwrap();

        let src_a = "module workspace.a { }";
        let src_b = "module workspace.b { requires dep; }";

        std::fs::write(mod_a.join("module-info.java"), src_a).unwrap();
        std::fs::write(mod_b.join("module-info.java"), src_b).unwrap();

        let info_a = lower_module_info_source_strict(src_a).unwrap();
        let info_b = lower_module_info_source_strict(src_b).unwrap();

        let ws = ProjectConfig {
            workspace_root: tmp.path().to_path_buf(),
            build_system: BuildSystem::Simple,
            java: JavaConfig::default(),
            modules: vec![Module {
                name: "dummy".to_string(),
                root: tmp.path().to_path_buf(),
                annotation_processing: Default::default(),
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
            ],
            jpms_workspace: None,
            source_roots: Vec::new(),
            module_path: Vec::new(),
            classpath: Vec::new(),
            output_dirs: Vec::new(),
            dependencies: Vec::new(),
            workspace_model: None,
        };

        let module_path = [ClasspathEntry::Jar(test_dep_jar())];
        let classpath: [ClasspathEntry; 0] = [];

        let jdk = JdkIndex::new();
        let env =
            build_jpms_compilation_environment(&jdk, Some(&ws), &module_path, &classpath, None)
                .unwrap();

        let ty = QualifiedName::from_dotted("com.example.dep.Foo");

        let from_a = ModuleName::new("workspace.a");
        assert!(
            !env.env.graph.can_read(&from_a, &env.env.unnamed),
            "workspace.a should not read the unnamed module when the classpath is empty"
        );
        let resolver_a = JpmsResolver::new(&jdk, &env.env.graph, &env.classpath, from_a);
        assert_eq!(resolver_a.resolve_qualified_name(&ty), None);

        let from_b = ModuleName::new("workspace.b");
        assert!(
            !env.env.graph.can_read(&from_b, &env.env.unnamed),
            "workspace.b should not read the unnamed module when the classpath is empty"
        );
        let resolver_b = JpmsResolver::new(&jdk, &env.env.graph, &env.classpath, from_b);
        assert_eq!(
            resolver_b.resolve_qualified_name(&ty),
            Some(TypeName::from("com.example.dep.Foo"))
        );
    }

    fn minimal_class_bytes(internal_name: &str, interfaces: &[&str]) -> Vec<u8> {
        fn push_u16(out: &mut Vec<u8>, value: u16) {
            out.extend_from_slice(&value.to_be_bytes());
        }
        fn push_u32(out: &mut Vec<u8>, value: u32) {
            out.extend_from_slice(&value.to_be_bytes());
        }
        fn push_utf8(out: &mut Vec<u8>, s: &str) {
            out.push(1); // CONSTANT_Utf8
            push_u16(out, s.len() as u16);
            out.extend_from_slice(s.as_bytes());
        }
        fn push_class(out: &mut Vec<u8>, name_index: u16) {
            out.push(7); // CONSTANT_Class
            push_u16(out, name_index);
        }

        const MAJOR_JAVA_8: u16 = 52;
        let super_internal = "java/lang/Object";

        // Constant pool:
        // 1: Utf8 this
        // 2: Class #1
        // 3: Utf8 super
        // 4: Class #3
        // 5+: (interfaces) Utf8 + Class pairs
        let cp_count: u16 = (4 + interfaces.len() * 2 + 1) as u16;

        let mut bytes = Vec::new();
        push_u32(&mut bytes, 0xCAFEBABE);
        push_u16(&mut bytes, 0); // minor
        push_u16(&mut bytes, MAJOR_JAVA_8);
        push_u16(&mut bytes, cp_count);

        push_utf8(&mut bytes, internal_name);
        push_class(&mut bytes, 1);
        push_utf8(&mut bytes, super_internal);
        push_class(&mut bytes, 3);

        let mut interface_class_indices: Vec<u16> = Vec::with_capacity(interfaces.len());
        for (i, interface) in interfaces.iter().enumerate() {
            let utf8_index = 5 + (i * 2) as u16;
            let class_index = utf8_index + 1;
            push_utf8(&mut bytes, interface);
            push_class(&mut bytes, utf8_index);
            interface_class_indices.push(class_index);
        }

        // access_flags (public + super)
        push_u16(&mut bytes, 0x0021);
        // this_class
        push_u16(&mut bytes, 2);
        // super_class
        push_u16(&mut bytes, 4);
        // interfaces_count
        push_u16(&mut bytes, interfaces.len() as u16);
        for idx in interface_class_indices {
            push_u16(&mut bytes, idx);
        }
        // fields_count, methods_count, attributes_count
        push_u16(&mut bytes, 0);
        push_u16(&mut bytes, 0);
        push_u16(&mut bytes, 0);

        bytes
    }

    fn write_multi_release_jar(
        jar_path: &std::path::Path,
        base_class_bytes: &[u8],
        mr_class_bytes: &[u8],
    ) {
        let file = std::fs::File::create(jar_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::FileOptions::default();

        zip.start_file("META-INF/MANIFEST.MF", options).unwrap();
        zip.write_all(b"Manifest-Version: 1.0\nMulti-Release: true\n")
            .unwrap();

        zip.start_file("com/example/mr/Override.class", options)
            .unwrap();
        zip.write_all(base_class_bytes).unwrap();

        zip.start_file("META-INF/versions/9/com/example/mr/Override.class", options)
            .unwrap();
        zip.write_all(mr_class_bytes).unwrap();

        zip.finish().unwrap();
    }

    #[test]
    fn compilation_environment_indexes_multi_release_jars_based_on_workspace_target_release() {
        let tmp = TempDir::new().unwrap();

        let jar_path = tmp.path().join("mr.jar");
        let internal_name = "com/example/mr/Override";
        let base_bytes = minimal_class_bytes(internal_name, &[]);
        let mr_bytes = minimal_class_bytes(internal_name, &["java/lang/Runnable"]);
        write_multi_release_jar(&jar_path, &base_bytes, &mr_bytes);

        let module_path = [ClasspathEntry::Jar(jar_path)];
        let classpath: [ClasspathEntry; 0] = [];

        let jdk = JdkIndex::new();
        assert_eq!(jdk.info().api_release, None);

        let root = tmp.path().to_path_buf();
        let mk_project = |target: JavaVersion| ProjectConfig {
            workspace_root: root.clone(),
            build_system: BuildSystem::Simple,
            java: JavaConfig {
                source: target,
                target,
                enable_preview: false,
            },
            modules: vec![Module {
                name: "dummy".to_string(),
                root: root.clone(),
                annotation_processing: Default::default(),
            }],
            jpms_modules: Vec::new(),
            jpms_workspace: None,
            source_roots: Vec::new(),
            module_path: Vec::new(),
            classpath: Vec::new(),
            output_dirs: Vec::new(),
            dependencies: Vec::new(),
            workspace_model: None,
        };

        let project_9 = mk_project(JavaVersion(9));
        let env_9 = build_jpms_compilation_environment(
            &jdk,
            Some(&project_9),
            &module_path,
            &classpath,
            None,
        )
        .unwrap();
        let stub_9 = env_9
            .classpath
            .types
            .lookup_binary("com.example.mr.Override")
            .expect("expected class to be indexed");
        assert_eq!(stub_9.interfaces, vec!["java.lang.Runnable".to_string()]);

        let project_8 = mk_project(JavaVersion::JAVA_8);
        let env_8 = build_jpms_compilation_environment(
            &jdk,
            Some(&project_8),
            &module_path,
            &classpath,
            None,
        )
        .unwrap();
        let stub_8 = env_8
            .classpath
            .types
            .lookup_binary("com.example.mr.Override")
            .expect("expected class to be indexed");
        assert!(stub_8.interfaces.is_empty());
    }
}
