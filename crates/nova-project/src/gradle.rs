use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;

use crate::discover::{LoadOptions, ProjectError};
use crate::{
    BuildSystem, ClasspathEntry, ClasspathEntryKind, Dependency, JavaConfig, JavaLanguageLevel,
    JavaVersion, LanguageLevelProvenance, Module, ModuleLanguageLevel, OutputDir, OutputDirKind,
    ProjectConfig, SourceRoot, SourceRootKind, SourceRootOrigin, WorkspaceModuleBuildId,
    WorkspaceModuleConfig, WorkspaceProjectModel,
};

pub(crate) fn load_gradle_project(
    root: &Path,
    options: &LoadOptions,
) -> Result<ProjectConfig, ProjectError> {
    let settings_path = ["settings.gradle.kts", "settings.gradle"]
        .into_iter()
        .map(|name| root.join(name))
        .find(|p| p.is_file());

    let module_names = if let Some(settings_path) = settings_path {
        let contents =
            std::fs::read_to_string(&settings_path).map_err(|source| ProjectError::Io {
                path: settings_path.clone(),
                source,
            })?;
        parse_gradle_settings_modules(&contents)
    } else {
        vec![".".to_string()]
    };

    let mut modules = Vec::new();
    let mut source_roots = Vec::new();
    let mut output_dirs = Vec::new();
    let mut dependencies = Vec::new();
    let mut classpath = Vec::new();
    let mut dependency_entries = Vec::new();

    // Best-effort: parse Java level and deps from build scripts.
    let root_java = parse_gradle_java_config(root).unwrap_or_default();

    for module_name in module_names {
        let module_root = if module_name == "." {
            root.to_path_buf()
        } else {
            root.join(&module_name)
        };
        let module_display_name = if module_name == "." {
            root.file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("root")
                .to_string()
        } else {
            module_name.clone()
        };

        modules.push(Module {
            name: module_display_name,
            root: module_root.clone(),
            annotation_processing: Default::default(),
        });

        let _module_java = parse_gradle_java_config(&module_root).unwrap_or(root_java);

        push_source_root(
            &mut source_roots,
            &module_root,
            SourceRootKind::Main,
            "src/main/java",
        );
        push_source_root(
            &mut source_roots,
            &module_root,
            SourceRootKind::Test,
            "src/test/java",
        );
        crate::generated::append_generated_source_roots(
            &mut source_roots,
            &module_root,
            BuildSystem::Gradle,
            &options.nova_config,
        );

        let main_output = module_root.join("build/classes/java/main");
        let test_output = module_root.join("build/classes/java/test");

        output_dirs.push(OutputDir {
            kind: OutputDirKind::Main,
            path: main_output.clone(),
        });
        output_dirs.push(OutputDir {
            kind: OutputDirKind::Test,
            path: test_output.clone(),
        });

        classpath.push(ClasspathEntry {
            kind: ClasspathEntryKind::Directory,
            path: main_output,
        });
        classpath.push(ClasspathEntry {
            kind: ClasspathEntryKind::Directory,
            path: test_output,
        });

        // Dependency extraction is best-effort; useful for later external jar resolution.
        dependencies.extend(parse_gradle_dependencies(&module_root));
    }

    // Add user-provided classpath entries for unresolved dependencies (Gradle).
    for entry in &options.classpath_overrides {
        dependency_entries.push(ClasspathEntry {
            kind: if entry.extension().is_some_and(|ext| ext == "jar") {
                ClasspathEntryKind::Jar
            } else {
                ClasspathEntryKind::Directory
            },
            path: entry.clone(),
        });
    }

    sort_dedup_source_roots(&mut source_roots);
    sort_dedup_output_dirs(&mut output_dirs);
    sort_dedup_classpath(&mut dependency_entries);
    sort_dedup_classpath(&mut classpath);
    sort_dedup_dependencies(&mut dependencies);

    let jpms_modules = crate::jpms::discover_jpms_modules(&modules);
    let (mut module_path, classpath_deps) =
        crate::jpms::classify_dependency_entries(&jpms_modules, dependency_entries);
    classpath.extend(classpath_deps);
    sort_dedup_classpath(&mut module_path);
    sort_dedup_classpath(&mut classpath);
    let jpms_workspace = crate::jpms::build_jpms_workspace(&jpms_modules, &module_path);

    Ok(ProjectConfig {
        workspace_root: root.to_path_buf(),
        build_system: BuildSystem::Gradle,
        java: root_java,
        modules,
        jpms_modules,
        jpms_workspace,
        source_roots,
        module_path,
        classpath,
        output_dirs,
        dependencies,
        workspace_model: None,
    })
}

pub(crate) fn load_gradle_workspace_model(
    root: &Path,
    options: &LoadOptions,
) -> Result<WorkspaceProjectModel, ProjectError> {
    let settings_path = ["settings.gradle.kts", "settings.gradle"]
        .into_iter()
        .map(|name| root.join(name))
        .find(|p| p.is_file());

    let module_refs = if let Some(settings_path) = settings_path {
        let contents =
            std::fs::read_to_string(&settings_path).map_err(|source| ProjectError::Io {
                path: settings_path.clone(),
                source,
            })?;
        parse_gradle_settings_projects(&contents)
    } else {
        vec![GradleModuleRef::root()]
    };

    let (root_java, root_java_provenance) = match parse_gradle_java_config_with_path(root) {
        Some((java, path)) => (java, LanguageLevelProvenance::BuildFile(path)),
        None => (JavaConfig::default(), LanguageLevelProvenance::Default),
    };

    let mut module_configs = Vec::new();
    for module_ref in module_refs {
        let module_root = if module_ref.dir_rel == "." {
            root.to_path_buf()
        } else {
            root.join(&module_ref.dir_rel)
        };

        let module_display_name = if module_ref.dir_rel == "." {
            root.file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("root")
                .to_string()
        } else {
            module_ref
                .project_path
                .trim_start_matches(':')
                .rsplit(':')
                .next()
                .unwrap_or(&module_ref.project_path)
                .to_string()
        };

        let (module_java, provenance) = match parse_gradle_java_config_with_path(&module_root) {
            Some((java, path)) => (java, LanguageLevelProvenance::BuildFile(path)),
            None => (root_java, root_java_provenance.clone()),
        };

        let language_level = ModuleLanguageLevel {
            level: JavaLanguageLevel::from_java_config(module_java),
            provenance,
        };

        let mut source_roots = Vec::new();
        push_source_root(
            &mut source_roots,
            &module_root,
            SourceRootKind::Main,
            "src/main/java",
        );
        push_source_root(
            &mut source_roots,
            &module_root,
            SourceRootKind::Test,
            "src/test/java",
        );
        crate::generated::append_generated_source_roots(
            &mut source_roots,
            &module_root,
            BuildSystem::Gradle,
            &options.nova_config,
        );

        let main_output = module_root.join("build/classes/java/main");
        let test_output = module_root.join("build/classes/java/test");
        let mut output_dirs = vec![
            OutputDir {
                kind: OutputDirKind::Main,
                path: main_output.clone(),
            },
            OutputDir {
                kind: OutputDirKind::Test,
                path: test_output.clone(),
            },
        ];

        let mut classpath = vec![
            ClasspathEntry {
                kind: ClasspathEntryKind::Directory,
                path: main_output,
            },
            ClasspathEntry {
                kind: ClasspathEntryKind::Directory,
                path: test_output,
            },
        ];

        for entry in &options.classpath_overrides {
            classpath.push(ClasspathEntry {
                kind: if entry.extension().is_some_and(|ext| ext == "jar") {
                    ClasspathEntryKind::Jar
                } else {
                    ClasspathEntryKind::Directory
                },
                path: entry.clone(),
            });
        }

        let mut dependencies = parse_gradle_dependencies(&module_root);

        sort_dedup_source_roots(&mut source_roots);
        sort_dedup_output_dirs(&mut output_dirs);
        sort_dedup_classpath(&mut classpath);
        sort_dedup_dependencies(&mut dependencies);

        module_configs.push(WorkspaceModuleConfig {
            id: format!("gradle:{}", module_ref.project_path),
            name: module_display_name,
            root: module_root,
            build_id: WorkspaceModuleBuildId::Gradle {
                project_path: module_ref.project_path,
            },
            language_level,
            source_roots,
            output_dirs,
            module_path: Vec::new(),
            classpath,
            dependencies,
        });
    }

    let modules_for_jpms = module_configs
        .iter()
        .map(|module| Module {
            name: module.name.clone(),
            root: module.root.clone(),
        })
        .collect::<Vec<_>>();
    let jpms_modules = crate::jpms::discover_jpms_modules(&modules_for_jpms);

    Ok(WorkspaceProjectModel::new(
        root.to_path_buf(),
        BuildSystem::Gradle,
        root_java,
        module_configs,
        jpms_modules,
    ))
}

fn parse_gradle_settings_modules(contents: &str) -> Vec<String> {
    // Very conservative: look for quoted strings in lines containing `include`.
    let mut modules = Vec::new();
    for line in contents.lines() {
        if !line.contains("include") {
            continue;
        }
        modules.extend(extract_quoted_strings(line).into_iter().map(|s| {
            let s = s.trim();
            let s = s.strip_prefix(':').unwrap_or(s);
            s.replace(':', "/")
        }));
    }

    if modules.is_empty() {
        vec![".".to_string()]
    } else {
        modules
    }
}

#[derive(Debug, Clone)]
struct GradleModuleRef {
    project_path: String,
    dir_rel: String,
}

impl GradleModuleRef {
    fn root() -> Self {
        Self {
            project_path: ":".to_string(),
            dir_rel: ".".to_string(),
        }
    }
}

fn parse_gradle_settings_projects(contents: &str) -> Vec<GradleModuleRef> {
    let mut modules: Vec<GradleModuleRef> = Vec::new();
    for line in contents.lines() {
        if !line.contains("include") {
            continue;
        }
        modules.extend(extract_quoted_strings(line).into_iter().map(|s| {
            let s = s.trim();
            let project_path = if s.starts_with(':') {
                s.to_string()
            } else {
                format!(":{s}")
            };
            let dir_rel = project_path
                .trim_start_matches(':')
                .replace(':', "/")
                .trim()
                .to_string();
            let dir_rel = if dir_rel.is_empty() {
                ".".to_string()
            } else {
                dir_rel
            };

            GradleModuleRef {
                project_path,
                dir_rel,
            }
        }));
    }

    if modules.is_empty() {
        vec![GradleModuleRef::root()]
    } else {
        modules
    }
}

fn extract_quoted_strings(text: &str) -> Vec<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r#"['"]([^'"]+)['"]"#).expect("valid regex"));

    re.captures_iter(text)
        .filter_map(|caps| caps.get(1).map(|m| m.as_str().to_string()))
        .collect()
}

fn parse_gradle_java_config(root: &Path) -> Option<JavaConfig> {
    let candidates = ["build.gradle.kts", "build.gradle"]
        .into_iter()
        .map(|name| root.join(name))
        .filter(|p| p.is_file())
        .collect::<Vec<_>>();

    for path in candidates {
        if let Ok(contents) = std::fs::read_to_string(&path) {
            if let Some(java) = extract_java_config_from_build_script(&contents) {
                return Some(java);
            }
        }
    }

    None
}

fn parse_gradle_java_config_with_path(root: &Path) -> Option<(JavaConfig, PathBuf)> {
    let candidates = ["build.gradle.kts", "build.gradle"]
        .into_iter()
        .map(|name| root.join(name))
        .filter(|p| p.is_file())
        .collect::<Vec<_>>();

    for path in candidates {
        if let Ok(contents) = std::fs::read_to_string(&path) {
            if let Some(java) = extract_java_config_from_build_script(&contents) {
                return Some((java, path));
            }
        }
    }

    None
}

fn extract_java_config_from_build_script(contents: &str) -> Option<JavaConfig> {
    let mut source = None;
    let mut target = None;

    for line in contents.lines() {
        if source.is_none() {
            source = parse_java_version_assignment(line, "sourceCompatibility");
        }
        if target.is_none() {
            target = parse_java_version_assignment(line, "targetCompatibility");
        }
        if source.is_some() && target.is_some() {
            break;
        }
    }

    match (source, target) {
        (Some(source), Some(target)) => Some(JavaConfig {
            source,
            target,
            enable_preview: false,
        }),
        (Some(v), None) | (None, Some(v)) => Some(JavaConfig {
            source: v,
            target: v,
            enable_preview: false,
        }),
        (None, None) => None,
    }
}

fn parse_java_version_assignment(line: &str, key: &str) -> Option<JavaVersion> {
    let line = line.trim();
    let rest = line.strip_prefix(key)?.trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();

    if let Some(rest) = rest.strip_prefix("JavaVersion.VERSION_") {
        let normalized = rest.trim().replace('_', ".");
        return JavaVersion::parse(&normalized);
    }

    let rest = rest.trim();
    let rest = rest
        .strip_prefix('"')
        .and_then(|v| v.split_once('"').map(|(head, _)| head))
        .or_else(|| {
            rest.strip_prefix('\'')
                .and_then(|v| v.split_once('\'').map(|(head, _)| head))
        })
        .unwrap_or(rest);

    JavaVersion::parse(rest)
}

fn parse_gradle_dependencies(module_root: &Path) -> Vec<Dependency> {
    let candidates = ["build.gradle.kts", "build.gradle"]
        .into_iter()
        .map(|name| module_root.join(name))
        .filter(|p| p.is_file())
        .collect::<Vec<_>>();

    let mut out = Vec::new();
    for path in candidates {
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };

        out.extend(parse_gradle_dependencies_from_text(&contents));
    }
    out
}

fn parse_gradle_dependencies_from_text(contents: &str) -> Vec<Dependency> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r#"(?i)\b(?:implementation|api|compileOnly|runtimeOnly|testImplementation)\s*\(?\s*['"]([^:'"]+):([^:'"]+):([^'"]+)['"]"#)
            .expect("valid regex")
    });

    let mut deps = Vec::new();
    for caps in re.captures_iter(contents) {
        deps.push(Dependency {
            group_id: caps[1].to_string(),
            artifact_id: caps[2].to_string(),
            version: Some(caps[3].to_string()),
            scope: None,
            classifier: None,
            type_: None,
        });
    }
    deps
}

fn push_source_root(
    out: &mut Vec<SourceRoot>,
    module_root: &Path,
    kind: SourceRootKind,
    rel: &str,
) {
    let path = module_root.join(rel);
    if path.is_dir() {
        out.push(SourceRoot {
            kind,
            origin: SourceRootOrigin::Source,
            path,
        });
    }
}

fn sort_dedup_source_roots(roots: &mut Vec<SourceRoot>) {
    roots.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then(a.kind.cmp(&b.kind))
            .then(a.origin.cmp(&b.origin))
    });
    roots.dedup_by(|a, b| a.kind == b.kind && a.origin == b.origin && a.path == b.path);
}

fn sort_dedup_output_dirs(dirs: &mut Vec<OutputDir>) {
    dirs.sort_by(|a, b| a.path.cmp(&b.path).then(a.kind.cmp(&b.kind)));
    dirs.dedup_by(|a, b| a.kind == b.kind && a.path == b.path);
}

fn sort_dedup_classpath(entries: &mut Vec<ClasspathEntry>) {
    entries.sort_by(|a, b| a.path.cmp(&b.path).then(a.kind.cmp(&b.kind)));
    entries.dedup_by(|a, b| a.kind == b.kind && a.path == b.path);
}

fn sort_dedup_dependencies(deps: &mut Vec<Dependency>) {
    deps.sort_by(|a, b| {
        a.group_id
            .cmp(&b.group_id)
            .then(a.artifact_id.cmp(&b.artifact_id))
            .then(a.version.cmp(&b.version))
    });
    deps.dedup();
}
