use std::path::Path;

use crate::discover::{LoadOptions, ProjectError};
use crate::{
    BuildSystem, ClasspathEntry, ClasspathEntryKind, JavaConfig, JavaLanguageLevel,
    LanguageLevelProvenance, Module, ModuleLanguageLevel, OutputDir, OutputDirKind, ProjectConfig,
    SourceRoot, SourceRootKind, SourceRootOrigin, WorkspaceModuleBuildId, WorkspaceModuleConfig,
    WorkspaceProjectModel,
};

#[cfg(feature = "bazel")]
use std::path::PathBuf;

#[cfg(feature = "bazel")]
use crate::{JavaVersion, ModuleConfig, WorkspaceModel};

#[cfg(feature = "bazel")]
use nova_build_bazel::{BazelWorkspace, CommandRunner, DefaultCommandRunner, JavaCompileInfo};

pub(crate) fn load_bazel_project(
    root: &Path,
    options: &LoadOptions,
) -> Result<ProjectConfig, ProjectError> {
    if options.bazel.enable_target_loading {
        #[cfg(feature = "bazel")]
        {
            let runner = DefaultCommandRunner::default();
            let model = load_bazel_workspace_model_with_runner(root, options, runner)?;
            if !model.modules.is_empty() {
                return Ok(project_config_from_workspace_model(root, options, model));
            }
        }
    }

    load_bazel_project_heuristic(root, options)
}

pub(crate) fn load_bazel_workspace_model(
    root: &Path,
    options: &LoadOptions,
) -> Result<WorkspaceProjectModel, ProjectError> {
    let mut source_roots = Vec::new();

    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }

        let name = entry.file_name().to_string_lossy();
        if name != "BUILD" && name != "BUILD.bazel" {
            continue;
        }

        let Some(dir) = entry.path().parent() else {
            continue;
        };

        source_roots.push(SourceRoot {
            kind: classify_bazel_source_root(dir),
            origin: SourceRootOrigin::Source,
            path: dir.to_path_buf(),
        });
    }

    if source_roots.is_empty() {
        source_roots.push(SourceRoot {
            kind: SourceRootKind::Main,
            origin: SourceRootOrigin::Source,
            path: root.to_path_buf(),
        });
    }

    crate::generated::append_generated_source_roots(
        &mut source_roots,
        root,
        BuildSystem::Bazel,
        &options.nova_config,
    );

    let mut classpath = Vec::new();
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

    sort_dedup_source_roots(&mut source_roots);
    sort_dedup_classpath(&mut classpath);

    let module_name = root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("root")
        .to_string();

    let module_config = WorkspaceModuleConfig {
        id: "bazel://".to_string(),
        name: module_name,
        root: root.to_path_buf(),
        build_id: WorkspaceModuleBuildId::Bazel {
            label: "//".to_string(),
        },
        language_level: ModuleLanguageLevel {
            level: JavaLanguageLevel::from_java_config(JavaConfig::default()),
            provenance: LanguageLevelProvenance::Default,
        },
        source_roots,
        output_dirs: Vec::new(),
        module_path: Vec::new(),
        classpath,
        dependencies: Vec::new(),
    };

    let jpms_modules = crate::jpms::discover_jpms_modules(&[Module {
        name: module_config.name.clone(),
        root: module_config.root.clone(),
    }]);

    Ok(WorkspaceProjectModel::new(
        root.to_path_buf(),
        BuildSystem::Bazel,
        JavaConfig::default(),
        vec![module_config],
        jpms_modules,
    ))
}

fn load_bazel_project_heuristic(
    root: &Path,
    options: &LoadOptions,
) -> Result<ProjectConfig, ProjectError> {
    let mut source_roots = Vec::new();

    // Naive Bazel heuristic:
    // - treat each package directory that contains a BUILD/BUILD.bazel file as a source root
    // - classify "test-ish" directories as test roots
    //
    // This is the default to avoid invoking Bazel unexpectedly.
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }

        let name = entry.file_name().to_string_lossy();
        if name != "BUILD" && name != "BUILD.bazel" {
            continue;
        }

        let Some(dir) = entry.path().parent() else {
            continue;
        };

        source_roots.push(SourceRoot {
            kind: classify_bazel_source_root(dir),
            origin: SourceRootOrigin::Source,
            path: dir.to_path_buf(),
        });
    }

    if source_roots.is_empty() {
        // Fallback: treat the workspace root as a source root so the rest of Nova can still
        // operate (e.g. file watchers, simple indexing).
        source_roots.push(SourceRoot {
            kind: SourceRootKind::Main,
            origin: SourceRootOrigin::Source,
            path: root.to_path_buf(),
        });
    }

    crate::generated::append_generated_source_roots(
        &mut source_roots,
        root,
        BuildSystem::Bazel,
        &options.nova_config,
    );

    let mut dependency_entries = Vec::new();
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
    sort_dedup_classpath(&mut dependency_entries);

    let modules = vec![Module {
        name: root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("root")
            .to_string(),
        root: root.to_path_buf(),
        annotation_processing: Default::default(),
    }];
    let jpms_modules = crate::jpms::discover_jpms_modules(&modules);
    let (mut module_path, mut classpath) =
        crate::jpms::classify_dependency_entries(&jpms_modules, dependency_entries);
    classpath.sort_by(|a, b| a.path.cmp(&b.path).then(a.kind.cmp(&b.kind)));
    classpath.dedup_by(|a, b| a.kind == b.kind && a.path == b.path);
    module_path.sort_by(|a, b| a.path.cmp(&b.path).then(a.kind.cmp(&b.kind)));
    module_path.dedup_by(|a, b| a.kind == b.kind && a.path == b.path);
    let jpms_workspace = crate::jpms::build_jpms_workspace(&jpms_modules, &module_path);

    Ok(ProjectConfig {
        workspace_root: root.to_path_buf(),
        build_system: BuildSystem::Bazel,
        java: JavaConfig::default(),
        modules,
        jpms_modules,
        jpms_workspace,
        source_roots,
        module_path,
        classpath,
        output_dirs: Vec::new(),
        dependencies: Vec::new(),
        workspace_model: None,
    })
}

#[cfg(feature = "bazel")]
pub fn load_bazel_workspace_model_with_runner<R: CommandRunner>(
    root: &Path,
    options: &LoadOptions,
    runner: R,
) -> Result<WorkspaceModel, ProjectError> {
    let cache_path = bazel_cache_path(root);

    let mut workspace = BazelWorkspace::new(root.to_path_buf(), runner)
        .and_then(|ws| ws.with_cache_path(cache_path))
        .map_err(|err| ProjectError::Bazel {
            message: err.to_string(),
        })?;

    let mut targets = workspace
        .java_targets()
        .map_err(|err| ProjectError::Bazel {
            message: err.to_string(),
        })?;
    targets.sort();
    targets.dedup();

    if let Some(wanted) = &options.bazel.targets {
        let wanted: std::collections::BTreeSet<&str> = wanted.iter().map(String::as_str).collect();
        targets.retain(|t| wanted.contains(t.as_str()));
    }

    let mut modules = Vec::new();
    for target in targets {
        if modules.len() >= options.bazel.target_limit {
            break;
        }
        let info = match workspace.target_compile_info(&target) {
            Ok(info) => info,
            Err(err) if is_target_without_javac_action(&err) => continue,
            Err(err) => {
                return Err(ProjectError::Bazel {
                    message: err.to_string(),
                })
            }
        };

        modules.push(module_config_from_compile_info(root, &target, &info));
    }

    modules.sort_by(|a, b| a.id.cmp(&b.id));
    modules.dedup_by(|a, b| a.id == b.id);

    Ok(WorkspaceModel { modules })
}

#[cfg(feature = "bazel")]
fn project_config_from_workspace_model(
    root: &Path,
    options: &LoadOptions,
    model: WorkspaceModel,
) -> ProjectConfig {
    let mut source_roots: Vec<SourceRoot> = model
        .modules
        .iter()
        .flat_map(|m| m.source_roots.iter().cloned())
        .collect();
    if source_roots.is_empty() {
        source_roots.push(SourceRoot {
            kind: SourceRootKind::Main,
            origin: SourceRootOrigin::Source,
            path: root.to_path_buf(),
        });
    }
    crate::generated::append_generated_source_roots(
        &mut source_roots,
        root,
        BuildSystem::Bazel,
        &options.nova_config,
    );

    let mut classpath: Vec<ClasspathEntry> = model
        .modules
        .iter()
        .flat_map(|m| m.classpath.iter().cloned())
        .collect();
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

    let mut module_path: Vec<ClasspathEntry> = model
        .modules
        .iter()
        .flat_map(|m| m.module_path.iter().cloned())
        .collect();

    let mut output_dirs: Vec<OutputDir> = model
        .modules
        .iter()
        .filter_map(|m| {
            let path = m.output_dir.clone()?;
            let kind = if m
                .source_roots
                .iter()
                .any(|r| r.kind == SourceRootKind::Test)
            {
                OutputDirKind::Test
            } else {
                OutputDirKind::Main
            };
            Some(OutputDir { kind, path })
        })
        .collect();

    sort_dedup_source_roots(&mut source_roots);
    sort_dedup_classpath(&mut classpath);
    sort_dedup_classpath(&mut module_path);
    sort_dedup_output_dirs(&mut output_dirs);

    let modules = vec![Module {
        name: root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("root")
            .to_string(),
        root: root.to_path_buf(),
        annotation_processing: Default::default(),
    }];
    let jpms_modules = crate::jpms::discover_jpms_modules(&modules);
    let jpms_workspace = crate::jpms::build_jpms_workspace(&jpms_modules, &module_path);

    ProjectConfig {
        workspace_root: root.to_path_buf(),
        build_system: BuildSystem::Bazel,
        java: JavaConfig::default(),
        modules,
        jpms_modules,
        jpms_workspace,
        source_roots,
        module_path,
        classpath,
        output_dirs,
        dependencies: Vec::new(),
        workspace_model: Some(model),
    }
}

#[cfg(feature = "bazel")]
fn module_config_from_compile_info(
    root: &Path,
    target: &str,
    info: &JavaCompileInfo,
) -> ModuleConfig {
    let mut source_roots = Vec::new();
    for rel in &info.source_roots {
        let kind = classify_bazel_source_root(Path::new(rel));
        source_roots.push(SourceRoot {
            kind,
            origin: SourceRootOrigin::Source,
            path: resolve_path(root, rel),
        });
    }

    let mut classpath = info
        .classpath
        .iter()
        .map(|entry| classpath_entry(root, entry))
        .collect::<Vec<_>>();
    let mut module_path = info
        .module_path
        .iter()
        .map(|entry| classpath_entry(root, entry))
        .collect::<Vec<_>>();

    sort_dedup_source_roots(&mut source_roots);
    sort_dedup_classpath(&mut classpath);
    sort_dedup_classpath(&mut module_path);

    let language_level = JavaLanguageLevel {
        release: info.release.as_deref().and_then(JavaVersion::parse),
        source: info.source.as_deref().and_then(JavaVersion::parse),
        target: info.target.as_deref().and_then(JavaVersion::parse),
        preview: info.preview,
    };

    ModuleConfig {
        id: target.to_string(),
        source_roots,
        classpath,
        module_path,
        language_level,
        output_dir: info.output_dir.as_deref().map(|p| resolve_path(root, p)),
    }
}

fn classify_bazel_source_root(path: &Path) -> SourceRootKind {
    let parts = path
        .components()
        .map(|component| component.as_os_str().to_string_lossy().to_lowercase())
        .collect::<Vec<_>>();

    if parts
        .iter()
        .any(|part| part == "test" || part == "tests" || part == "javatests")
    {
        SourceRootKind::Test
    } else {
        SourceRootKind::Main
    }
}

#[cfg(feature = "bazel")]
fn resolve_path(root: &Path, path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
}

#[cfg(feature = "bazel")]
fn classpath_entry(root: &Path, raw: &str) -> ClasspathEntry {
    let path = resolve_path(root, raw);
    let kind = if raw.ends_with(".jar") {
        ClasspathEntryKind::Jar
    } else {
        ClasspathEntryKind::Directory
    };
    ClasspathEntry { kind, path }
}

#[cfg(feature = "bazel")]
fn is_target_without_javac_action(err: &dyn std::fmt::Display) -> bool {
    err.to_string().contains("no Javac actions found")
}

#[cfg(feature = "bazel")]
fn bazel_cache_path(root: &Path) -> PathBuf {
    root.join(".nova").join("queries").join("bazel.json")
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

fn sort_dedup_classpath(entries: &mut Vec<ClasspathEntry>) {
    entries.sort_by(|a, b| a.path.cmp(&b.path).then(a.kind.cmp(&b.kind)));
    entries.dedup_by(|a, b| a.kind == b.kind && a.path == b.path);
}

fn sort_dedup_output_dirs(dirs: &mut Vec<OutputDir>) {
    dirs.sort_by(|a, b| a.path.cmp(&b.path).then(a.kind.cmp(&b.kind)));
    dirs.dedup_by(|a, b| a.kind == b.kind && a.path == b.path);
}
