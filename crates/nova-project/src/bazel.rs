use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

use crate::discover::{LoadOptions, ProjectError};
use crate::{
    BuildSystem, ClasspathEntry, ClasspathEntryKind, JavaConfig, JavaLanguageLevel,
    LanguageLevelProvenance, Module, ModuleLanguageLevel, OutputDir, OutputDirKind, ProjectConfig,
    SourceRoot, SourceRootKind, SourceRootOrigin, WorkspaceModuleBuildId, WorkspaceModuleConfig,
    WorkspaceProjectModel,
};

#[cfg(feature = "bazel")]
use crate::{JavaVersion, ModuleConfig, WorkspaceModel};

#[cfg(feature = "bazel")]
use nova_build_bazel::{BazelWorkspace, CommandRunner, DefaultCommandRunner, JavaCompileInfo};

#[cfg(feature = "bazel")]
fn contains_serde_json_error(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(err) = current {
        if err.is::<serde_json::Error>() {
            return true;
        }

        if let Some(io_err) = err.downcast_ref::<std::io::Error>() {
            if let Some(inner) = io_err.get_ref() {
                let inner: &(dyn std::error::Error + 'static) = inner;
                if contains_serde_json_error(inner) {
                    return true;
                }
            }
        }

        current = err.source();
    }
    false
}

pub(crate) fn load_bazel_project(
    root: &Path,
    options: &LoadOptions,
) -> Result<ProjectConfig, ProjectError> {
    if options.bazel.enable_target_loading {
        #[cfg(feature = "bazel")]
        {
            let runner = DefaultCommandRunner;
            let model = load_bazel_workspace_model_with_runner(root, options, runner)?;
            if !model.modules.is_empty() {
                return Ok(project_config_from_workspace_model(root, options, model));
            }
        }
        #[cfg(not(feature = "bazel"))]
        {
            return Err(ProjectError::Bazel {
                message: "Bazel target loading requires enabling the `nova-project/bazel` feature"
                    .to_string(),
            });
        }
    }

    load_bazel_project_heuristic(root, options)
}

pub(crate) fn load_bazel_workspace_model(
    root: &Path,
    options: &LoadOptions,
) -> Result<WorkspaceProjectModel, ProjectError> {
    if options.bazel.enable_target_loading {
        #[cfg(feature = "bazel")]
        {
            let runner = DefaultCommandRunner;
            let model = load_bazel_workspace_project_model_with_runner(root, options, runner)?;
            if !model.modules.is_empty() {
                return Ok(model);
            }
        }
        #[cfg(not(feature = "bazel"))]
        {
            return Err(ProjectError::Bazel {
                message: "Bazel target loading requires enabling the `nova-project/bazel` feature"
                    .to_string(),
            });
        }
    }

    let mut source_roots = discover_bazel_source_roots_heuristic(root);

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
        root,
        BuildSystem::Bazel,
        &options.nova_config,
    );

    let mut dependency_entries = Vec::new();
    for entry in &options.classpath_overrides {
        dependency_entries.push(ClasspathEntry {
            kind: if entry
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| {
                    ext.eq_ignore_ascii_case("jar") || ext.eq_ignore_ascii_case("jmod")
                }) {
                ClasspathEntryKind::Jar
            } else {
                ClasspathEntryKind::Directory
            },
            path: entry.clone(),
        });
    }

    sort_dedup_source_roots(&mut source_roots);
    sort_dedup_classpath(&mut dependency_entries);

    let module_name = root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("root")
        .to_string();

    let jpms_modules = crate::jpms::discover_jpms_modules(&[Module {
        name: module_name.clone(),
        root: root.to_path_buf(),
        annotation_processing: Default::default(),
    }]);

    let (mut module_path, mut classpath) =
        crate::jpms::classify_dependency_entries(&jpms_modules, dependency_entries);
    sort_dedup_classpath(&mut module_path);
    sort_dedup_classpath(&mut classpath);

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
        module_path,
        classpath,
        dependencies: Vec::new(),
    };

    Ok(WorkspaceProjectModel::new(
        root.to_path_buf(),
        BuildSystem::Bazel,
        JavaConfig::default(),
        vec![module_config],
        jpms_modules,
    ))
}

const BAZEL_ALWAYS_IGNORED_DIRS: [&str; 9] = [
    ".git",
    ".hg",
    ".svn",
    ".idea",
    ".vscode",
    ".nova",
    "target",
    "build",
    "node_modules",
];

fn bazel_ignored_path_prefixes(workspace_root: &Path) -> BTreeSet<PathBuf> {
    let mut ignored = BTreeSet::new();
    ignored.extend(BAZEL_ALWAYS_IGNORED_DIRS.iter().map(PathBuf::from));

    let bazelignore_path = workspace_root.join(".bazelignore");
    let Ok(contents) = std::fs::read_to_string(&bazelignore_path) else {
        return ignored;
    };

    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // `.bazelignore` entries are workspace-root-relative. We normalize the path to something we
        // can match using `Path::starts_with`.
        let line = line.trim_start_matches("./");
        let line = line.trim_start_matches('/');
        let line = line.trim_start_matches('\\');

        let mut normalized = PathBuf::new();
        for component in Path::new(line).components() {
            match component {
                Component::CurDir => {}
                Component::Normal(part) => normalized.push(part),
                // Ignore entries that escape the workspace root (or are absolute).
                Component::ParentDir | Component::Prefix(_) | Component::RootDir => {
                    normalized = PathBuf::new();
                    break;
                }
            }
        }

        if !normalized.as_os_str().is_empty() {
            ignored.insert(normalized);
        }
    }

    ignored
}

fn bazel_walkdir_filter_entry(
    workspace_root: &Path,
    ignored_prefixes: &BTreeSet<PathBuf>,
    entry: &walkdir::DirEntry,
) -> bool {
    if entry.depth() == 0 {
        return true;
    }

    // Bazel creates a symlink farm (or directories on some platforms) in the workspace root
    // containing output artifacts (e.g. `bazel-out`, `bazel-bin`, `bazel-testlogs`, and
    // `bazel-<workspace>`). These trees can be huge and may contain non-source BUILD files.
    if entry.depth() == 1
        && entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with("bazel-"))
    {
        return false;
    }

    // Fast path: prune common junk directories even when nested (e.g. `.git` submodules, nested
    // `target/` dirs, etc).
    if entry.file_type().is_dir()
        && entry
            .file_name()
            .to_str()
            .is_some_and(|name| BAZEL_ALWAYS_IGNORED_DIRS.contains(&name))
    {
        return false;
    }

    let Ok(rel) = entry.path().strip_prefix(workspace_root) else {
        return true;
    };

    !ignored_prefixes
        .iter()
        .any(|prefix| rel.starts_with(prefix))
}

fn discover_bazel_source_roots_heuristic(workspace_root: &Path) -> Vec<SourceRoot> {
    let ignored_prefixes = bazel_ignored_path_prefixes(workspace_root);

    let mut source_roots = Vec::new();
    for entry in walkdir::WalkDir::new(workspace_root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| bazel_walkdir_filter_entry(workspace_root, &ignored_prefixes, entry))
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }

        let name = entry.file_name();
        if name != OsStr::new("BUILD") && name != OsStr::new("BUILD.bazel") {
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

    source_roots
}

fn load_bazel_project_heuristic(
    root: &Path,
    options: &LoadOptions,
) -> Result<ProjectConfig, ProjectError> {
    // Naive Bazel heuristic:
    // - treat each package directory that contains a BUILD/BUILD.bazel file as a source root
    // - classify "test-ish" directories as test roots
    //
    // This is the default to avoid invoking Bazel unexpectedly.
    let mut source_roots = discover_bazel_source_roots_heuristic(root);

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
        root,
        BuildSystem::Bazel,
        &options.nova_config,
    );

    let mut dependency_entries = Vec::new();
    for entry in &options.classpath_overrides {
        dependency_entries.push(ClasspathEntry {
            kind: if entry
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| {
                    ext.eq_ignore_ascii_case("jar") || ext.eq_ignore_ascii_case("jmod")
                }) {
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
        .map_err(|err| {
            let contains_serde_json = err.chain().any(contains_serde_json_error);
            let message = err.to_string();
            let message = nova_core::sanitize_error_message_text(&message, contains_serde_json);
            ProjectError::Bazel { message }
        })?;

    // Best-effort: `bazel info execution_root` can fail in some environments; fall back to
    // workspace-root-relative resolution if it does.
    let execution_root = workspace.execution_root().ok();

    let mut targets = match options.bazel.target_universe.as_deref() {
        Some(universe) => workspace.java_targets_in_universe(universe),
        None => workspace.java_targets(),
    }
    .map_err(|err| ProjectError::Bazel {
        message: {
            let contains_serde_json = err.chain().any(contains_serde_json_error);
            let message = err.to_string();
            nova_core::sanitize_error_message_text(&message, contains_serde_json)
        },
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
                    message: {
                        let contains_serde_json = err.chain().any(contains_serde_json_error);
                        let message = err.to_string();
                        nova_core::sanitize_error_message_text(&message, contains_serde_json)
                    },
                })
            }
        };

        modules.push(module_config_from_compile_info(
            root,
            execution_root.as_deref(),
            &target,
            &info,
        ));
    }

    modules.sort_by(|a, b| a.id.cmp(&b.id));
    modules.dedup_by(|a, b| a.id == b.id);

    Ok(WorkspaceModel { modules })
}

#[cfg(feature = "bazel")]
pub fn load_bazel_workspace_project_model_with_runner<R: CommandRunner>(
    root: &Path,
    options: &LoadOptions,
    runner: R,
) -> Result<WorkspaceProjectModel, ProjectError> {
    let model = load_bazel_workspace_model_with_runner(root, options, runner)?;
    let java = workspace_java_from_target_modules(&model.modules);

    let mut modules = model
        .modules
        .iter()
        .map(|module| workspace_module_config_from_module_config(root, options, module))
        .collect::<Vec<_>>();
    modules.sort_by(|a, b| a.id.cmp(&b.id));
    modules.dedup_by(|a, b| a.id == b.id);

    let modules_for_jpms = modules
        .iter()
        .map(|module| Module {
            name: module.name.clone(),
            root: module.root.clone(),
            annotation_processing: Default::default(),
        })
        .collect::<Vec<_>>();
    let jpms_modules = crate::jpms::discover_jpms_modules(&modules_for_jpms);

    // Add user-provided dependency entries (overrides) after JPMS discovery so we can decide
    // whether they should land on the module-path or classpath. Only overrides are
    // subject to reclassification; Bazel-provided module/classpath entries remain untouched.
    let mut dependency_entries = Vec::new();
    for entry in &options.classpath_overrides {
        dependency_entries.push(ClasspathEntry {
            kind: if entry
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| {
                    ext.eq_ignore_ascii_case("jar") || ext.eq_ignore_ascii_case("jmod")
                }) {
                ClasspathEntryKind::Jar
            } else {
                ClasspathEntryKind::Directory
            },
            path: entry.clone(),
        });
    }
    sort_dedup_classpath(&mut dependency_entries);
    let (module_path_deps, classpath_deps) =
        crate::jpms::classify_dependency_entries(&jpms_modules, dependency_entries);

    for module in &mut modules {
        module.module_path.extend(module_path_deps.iter().cloned());
        module.classpath.extend(classpath_deps.iter().cloned());
        sort_dedup_classpath(&mut module.module_path);
        sort_dedup_classpath(&mut module.classpath);
    }

    Ok(WorkspaceProjectModel::new(
        root.to_path_buf(),
        BuildSystem::Bazel,
        java,
        modules,
        jpms_modules,
    ))
}

#[cfg(feature = "bazel")]
fn project_config_from_workspace_model(
    root: &Path,
    options: &LoadOptions,
    model: WorkspaceModel,
) -> ProjectConfig {
    let java = workspace_java_from_target_modules(&model.modules);

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
            kind: if entry
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| {
                    ext.eq_ignore_ascii_case("jar") || ext.eq_ignore_ascii_case("jmod")
                }) {
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
        java,
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
    execution_root: Option<&Path>,
    target: &str,
    info: &JavaCompileInfo,
) -> ModuleConfig {
    let aquery_root = execution_root.unwrap_or(root);

    let mut source_roots = Vec::new();
    for rel in &info.source_roots {
        let kind = classify_bazel_source_root(Path::new(rel));
        source_roots.push(SourceRoot {
            kind,
            origin: SourceRootOrigin::Source,
            path: resolve_path(root, rel),
        });
    }

    if let Some(annotation_processing) = &info.annotation_processing {
        if let Some(generated_sources_dir) = &annotation_processing.generated_sources_dir {
            // Heuristic: when a Bazel target has any test source roots, treat the annotation
            // processor output as test sources too. Otherwise mark it as main.
            let kind = if source_roots
                .iter()
                .any(|root| root.kind == SourceRootKind::Test)
            {
                SourceRootKind::Test
            } else {
                SourceRootKind::Main
            };

            let path = if generated_sources_dir.is_absolute() {
                generated_sources_dir.clone()
            } else {
                root.join(generated_sources_dir)
            };

            source_roots.push(SourceRoot {
                kind,
                origin: SourceRootOrigin::Generated,
                path,
            });
        }
    }

    let mut classpath = info
        .classpath
        .iter()
        .map(|entry| classpath_entry(aquery_root, entry))
        .collect::<Vec<_>>();
    let mut module_path = info
        .module_path
        .iter()
        .map(|entry| classpath_entry(aquery_root, entry))
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
        output_dir: info
            .output_dir
            .as_deref()
            .map(|p| resolve_path(aquery_root, p)),
    }
}

#[cfg(feature = "bazel")]
fn workspace_java_from_target_modules(modules: &[ModuleConfig]) -> JavaConfig {
    let mut max_source: Option<JavaVersion> = None;
    let mut max_target: Option<JavaVersion> = None;
    let mut enable_preview = false;

    for module in modules {
        let level = &module.language_level;
        let source = level.source.or(level.release);
        let target = level.target.or(level.release);

        if let Some(version) = source {
            max_source = Some(max_source.map_or(version, |current| current.max(version)));
        }
        if let Some(version) = target {
            max_target = Some(max_target.map_or(version, |current| current.max(version)));
        }
        if level.preview {
            enable_preview = true;
        }
    }

    let mut java = JavaConfig::default();
    if let Some(source) = max_source {
        java.source = source;
    }
    if let Some(target) = max_target {
        java.target = target;
    }
    java.enable_preview = enable_preview;
    java
}

#[cfg(feature = "bazel")]
fn workspace_module_config_from_module_config(
    workspace_root: &Path,
    options: &LoadOptions,
    module: &ModuleConfig,
) -> WorkspaceModuleConfig {
    let root = workspace_root_for_target(workspace_root, &module.id);
    let name = bazel_target_display_name(&module.id);

    let mut source_roots = module.source_roots.clone();
    crate::generated::append_generated_source_roots(
        &mut source_roots,
        workspace_root,
        &root,
        BuildSystem::Bazel,
        &options.nova_config,
    );

    let mut classpath = module.classpath.clone();

    let mut module_path = module.module_path.clone();

    let mut output_dirs = Vec::new();
    if let Some(path) = &module.output_dir {
        let kind = if module
            .source_roots
            .iter()
            .any(|root| root.kind == SourceRootKind::Test)
        {
            OutputDirKind::Test
        } else {
            OutputDirKind::Main
        };
        output_dirs.push(OutputDir {
            kind,
            path: path.clone(),
        });
    }

    sort_dedup_source_roots(&mut source_roots);
    sort_dedup_classpath(&mut classpath);
    sort_dedup_classpath(&mut module_path);
    sort_dedup_output_dirs(&mut output_dirs);

    WorkspaceModuleConfig {
        id: module.id.clone(),
        name,
        root,
        build_id: WorkspaceModuleBuildId::Bazel {
            label: module.id.clone(),
        },
        language_level: ModuleLanguageLevel {
            level: module.language_level.clone(),
            provenance: LanguageLevelProvenance::Default,
        },
        source_roots,
        output_dirs,
        module_path,
        classpath,
        dependencies: Vec::new(),
    }
}

#[cfg(feature = "bazel")]
fn workspace_root_for_target(workspace_root: &Path, target: &str) -> PathBuf {
    let Some(package) = target.strip_prefix("//") else {
        return workspace_root.to_path_buf();
    };
    let package = package.split(':').next().unwrap_or(package);
    if package.is_empty() {
        workspace_root.to_path_buf()
    } else {
        workspace_root.join(package)
    }
}

#[cfg(feature = "bazel")]
fn bazel_target_display_name(target: &str) -> String {
    if let Some((_, name)) = target.rsplit_once(':') {
        return name.to_string();
    }
    target.rsplit('/').next().unwrap_or(target).to_string()
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
