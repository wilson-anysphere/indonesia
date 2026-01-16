use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use nova_build_model::groovy_scan::is_index_inside_string_ranges;
use nova_build_model::{
    collect_gradle_build_files, is_gradle_marker_root, strip_gradle_comments, BuildFileFingerprint,
    GradleSnapshotFile, GradleSnapshotJavaCompileConfig, GRADLE_SNAPSHOT_REL_PATH,
    GRADLE_SNAPSHOT_SCHEMA_VERSION,
};
use regex::Regex;
use toml::Value;
use walkdir::WalkDir;

use crate::discover::{LoadOptions, ProjectError};
use crate::{
    BuildSystem, ClasspathEntry, ClasspathEntryKind, Dependency, JavaConfig, JavaLanguageLevel,
    JavaVersion, LanguageLevelProvenance, Module, ModuleLanguageLevel, OutputDir, OutputDirKind,
    ProjectConfig, SourceRoot, SourceRootKind, SourceRootOrigin, WorkspaceModuleBuildId,
    WorkspaceModuleConfig, WorkspaceProjectModel,
};

fn root_project_has_sources(root: &Path) -> bool {
    // Standard Gradle layout.
    if root.join("src/main/java").is_dir() || root.join("src/test/java").is_dir() {
        return true;
    }

    // Custom Gradle source sets (e.g. `src/integrationTest/java`).
    let src_dir = root.join("src");
    let Ok(entries) = std::fs::read_dir(&src_dir) else {
        return false;
    };

    entries.filter_map(|entry| entry.ok()).any(|entry| {
        let Ok(source_set) = entry.file_name().into_string() else {
            return false;
        };
        src_dir.join(source_set).join("java").is_dir()
    })
}

/// Stable synthetic project path for Gradle's special `buildSrc/` build.
///
/// `buildSrc` is not a normal Gradle subproject and does not appear in `settings.gradle`,
/// but it is compiled by Gradle and often contains build logic developers want indexed.
const GRADLE_BUILDSRC_PROJECT_PATH: &str = ":__buildSrc";

fn maybe_insert_buildsrc_module_ref(
    module_refs: &mut Vec<GradleModuleRef>,
    workspace_root: &Path,
    snapshot: Option<&GradleSnapshotFile>,
) {
    let buildsrc_root = workspace_root.join("buildSrc");
    if !buildsrc_root.is_dir() {
        return;
    }

    // `buildSrc` can be either a single-module build (sources under `buildSrc/src/**`) or a
    // multi-project build (sources only in subprojects declared by `buildSrc/settings.gradle(.kts)`).
    // Include `buildSrc` as a module whenever it (or any of its subprojects) contains Java sources
    // so Nova can index build logic sources.
    let mut has_sources = root_project_has_sources(&buildsrc_root);
    if !has_sources {
        let buildsrc_settings_path = ["settings.gradle.kts", "settings.gradle"]
            .into_iter()
            .map(|name| buildsrc_root.join(name))
            .find(|p| p.is_file());
        if let Some(settings_path) = buildsrc_settings_path {
            if let Ok(contents) = std::fs::read_to_string(&settings_path) {
                for module_ref in parse_gradle_settings_projects(&contents) {
                    let module_root = if module_ref.dir_rel == "." {
                        buildsrc_root.clone()
                    } else {
                        buildsrc_root.join(&module_ref.dir_rel)
                    };
                    if root_project_has_sources(&module_root) {
                        has_sources = true;
                        break;
                    }
                }
            }
        }
    }

    let include_from_snapshot = snapshot.is_some_and(|snapshot| {
        snapshot
            .java_compile_configs
            .contains_key(GRADLE_BUILDSRC_PROJECT_PATH)
            || snapshot
                .projects
                .iter()
                .any(|p| p.path == GRADLE_BUILDSRC_PROJECT_PATH)
    });

    if !has_sources && !include_from_snapshot {
        return;
    }

    // Avoid collisions/duplicates if the workspace already contains a project with the same
    // synthetic path or an explicit `buildSrc` module mapping.
    if module_refs
        .iter()
        .any(|m| m.project_path == GRADLE_BUILDSRC_PROJECT_PATH || m.dir_rel == "buildSrc")
    {
        return;
    }

    // Deterministic ordering:
    // - root project first (if present)
    // - buildSrc next
    // - other subprojects after
    let insert_idx = module_refs
        .iter()
        .position(|m| m.dir_rel == ".")
        .map(|idx| idx + 1)
        .unwrap_or(0);

    module_refs.insert(
        insert_idx,
        GradleModuleRef {
            project_path: GRADLE_BUILDSRC_PROJECT_PATH.to_string(),
            dir_rel: "buildSrc".to_string(),
        },
    );
}
fn gradle_build_fingerprint(project_root: &Path) -> std::io::Result<BuildFileFingerprint> {
    let build_files = collect_gradle_build_files(project_root)?;
    BuildFileFingerprint::from_files(project_root, build_files)
}

fn gradle_snapshot_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(GRADLE_SNAPSHOT_REL_PATH)
}

fn load_gradle_snapshot(workspace_root: &Path) -> Option<GradleSnapshotFile> {
    let path = gradle_snapshot_path(workspace_root);
    let bytes = std::fs::read(&path).ok()?;
    let snapshot: GradleSnapshotFile = serde_json::from_slice(&bytes).ok()?;
    if snapshot.schema_version != GRADLE_SNAPSHOT_SCHEMA_VERSION {
        return None;
    }

    let fingerprint = gradle_build_fingerprint(workspace_root).ok()?;
    if snapshot.build_fingerprint != fingerprint.digest {
        return None;
    }

    Some(snapshot)
}

fn resolve_snapshot_project_dir(workspace_root: &Path, dir: &Path) -> PathBuf {
    if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        workspace_root.join(dir)
    }
}

fn classpath_entry_kind_for_path(path: &Path) -> ClasspathEntryKind {
    if path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("jar") || ext.eq_ignore_ascii_case("jmod"))
    {
        return ClasspathEntryKind::Jar;
    }

    // Best-effort: Gradle classpaths often include output directories that might not exist yet
    // (pre-build). Treat non-`.jar` entries as directories to avoid misclassifying outputs.
    ClasspathEntryKind::Directory
}

fn java_language_level_from_snapshot(cfg: &GradleSnapshotJavaCompileConfig) -> JavaLanguageLevel {
    JavaLanguageLevel {
        release: cfg.release.as_deref().and_then(JavaVersion::parse),
        source: cfg.source.as_deref().and_then(JavaVersion::parse),
        target: cfg.target.as_deref().and_then(JavaVersion::parse),
        preview: cfg.enable_preview,
    }
}

fn java_config_from_snapshot(snapshot: &GradleSnapshotFile) -> Option<JavaConfig> {
    let mut source: Option<JavaVersion> = None;
    let mut target: Option<JavaVersion> = None;
    let mut enable_preview = false;

    for cfg in snapshot.java_compile_configs.values() {
        enable_preview |= cfg.enable_preview;

        // `--release` implies both source + target.
        let release = cfg.release.as_deref().and_then(JavaVersion::parse);
        let cfg_source = cfg.source.as_deref().and_then(JavaVersion::parse);
        let cfg_target = cfg.target.as_deref().and_then(JavaVersion::parse);

        if let Some(v) = release.or(cfg_source) {
            source = Some(source.map_or(v, |cur| cur.max(v)));
        }
        if let Some(v) = release.or(cfg_target) {
            target = Some(target.map_or(v, |cur| cur.max(v)));
        }
    }

    if source.is_none() && target.is_none() && !enable_preview {
        return None;
    }

    Some(JavaConfig {
        source: source.unwrap_or(JavaVersion::JAVA_17),
        target: target.unwrap_or(JavaVersion::JAVA_17),
        enable_preview,
    })
}

pub(crate) fn load_gradle_project(
    root: &Path,
    options: &LoadOptions,
) -> Result<ProjectConfig, ProjectError> {
    let settings_path = ["settings.gradle.kts", "settings.gradle"]
        .into_iter()
        .map(|name| root.join(name))
        .find(|p| p.is_file());

    let (mut module_refs, include_builds) = if let Some(settings_path) = settings_path.as_ref() {
        let contents =
            std::fs::read_to_string(settings_path).map_err(|source| ProjectError::Io {
                path: settings_path.clone(),
                source,
            })?;
        (
            parse_gradle_settings_projects(&contents),
            parse_gradle_settings_included_builds(&contents),
        )
    } else {
        (vec![GradleModuleRef::root()], Vec::new())
    };

    // When a Gradle workspace defines subprojects in `settings.gradle(.kts)`, we usually treat the
    // included projects as the workspace modules (excluding the root project). However, some
    // workspaces also keep Java sources in the root project; in that case we include the root as a
    // module too. For determinism, we always put the root module first.
    if settings_path.is_some()
        && root_project_has_sources(root)
        && !module_refs.iter().any(|module| module.dir_rel == ".")
    {
        module_refs.insert(0, GradleModuleRef::root());
    }

    let snapshot = load_gradle_snapshot(root);
    maybe_insert_buildsrc_module_ref(&mut module_refs, root, snapshot.as_ref());
    let buildsrc_builds: Vec<GradleModuleRef> = module_refs
        .iter()
        .filter(|m| m.project_path == GRADLE_BUILDSRC_PROJECT_PATH)
        .cloned()
        .collect();
    let included_builds = append_included_build_module_refs(&mut module_refs, root, include_builds);
    append_included_build_subproject_module_refs(&mut module_refs, root, &included_builds);
    append_included_build_subproject_module_refs(&mut module_refs, root, &buildsrc_builds);

    let mut snapshot_project_dirs: HashMap<String, PathBuf> = HashMap::new();
    if let Some(snapshot) = snapshot.as_ref() {
        for project in &snapshot.projects {
            snapshot_project_dirs.insert(
                project.path.clone(),
                resolve_snapshot_project_dir(root, &project.project_dir),
            );
        }
        // Redundant projectDir copy in `javaCompileConfigs`.
        for (project_path, cfg) in &snapshot.java_compile_configs {
            snapshot_project_dirs
                .entry(project_path.clone())
                .or_insert_with(|| resolve_snapshot_project_dir(root, &cfg.project_dir));
        }
    }

    let mut modules = Vec::new();
    let mut source_roots = Vec::new();
    let mut output_dirs = Vec::new();
    let mut dependencies = Vec::new();
    let mut module_path = Vec::new();
    let mut classpath = Vec::new();
    let mut dependency_entries = Vec::new();

    // Best-effort Gradle cache resolution. This does not execute Gradle; it only
    // adds jars that already exist in the local Gradle cache.
    let gradle_user_home = options
        .gradle_user_home
        .clone()
        .or_else(default_gradle_user_home);

    // Best-effort: parse Java level and deps from build scripts.
    //
    // When a Gradle snapshot exists, its Java config (if present) is authoritative.
    // Otherwise, we aggregate Java level across discovered modules by taking the max
    // `source`/`target` and OR-ing `enable_preview`.
    let mut root_java = parse_gradle_java_config(root).unwrap_or_default();
    let snapshot_java = snapshot.as_ref().and_then(java_config_from_snapshot);
    let aggregate_java_across_modules = snapshot_java.is_none();
    if let Some(java) = snapshot_java {
        root_java = java;
    }
    let mut workspace_java = root_java;

    struct GradleBuildContext {
        project_path_prefix: String,
        build_root: PathBuf,
        gradle_properties: GradleProperties,
        version_catalog: Option<GradleVersionCatalog>,
    }

    // Included builds (`includeBuild(...)`) are separate Gradle builds and can have their own
    // `gradle.properties` / version catalogs. Collect per-build parsing contexts so we can resolve:
    // - `libs.*` references inside build scripts, and
    // - root-level `subprojects { ... }` / `allprojects { ... }` dependencies.
    //
    // NOTE: keep the context matching logic consistent with `load_gradle_workspace_model`.
    let mut build_contexts: Vec<GradleBuildContext> = Vec::new();
    let root_gradle_properties = load_gradle_properties(root);
    let root_version_catalog = load_gradle_version_catalog(root, &root_gradle_properties);
    build_contexts.push(GradleBuildContext {
        project_path_prefix: ":".to_string(),
        build_root: canonicalize_or_fallback(root),
        gradle_properties: root_gradle_properties,
        version_catalog: root_version_catalog,
    });

    build_contexts.extend(included_builds.iter().map(|included| {
        let build_root = canonicalize_or_fallback(&root.join(&included.dir_rel));
        let props = load_gradle_properties(&build_root);
        let catalog = load_gradle_version_catalog(&build_root, &props);
        GradleBuildContext {
            project_path_prefix: included.project_path.clone(),
            build_root,
            gradle_properties: props,
            version_catalog: catalog,
        }
    }));
    if module_refs
        .iter()
        .any(|m| m.project_path == GRADLE_BUILDSRC_PROJECT_PATH)
    {
        let buildsrc_root = snapshot_project_dirs
            .get(GRADLE_BUILDSRC_PROJECT_PATH)
            .cloned()
            .unwrap_or_else(|| root.join("buildSrc"));
        let buildsrc_root = canonicalize_or_fallback(&buildsrc_root);
        let props = load_gradle_properties(&buildsrc_root);
        let catalog = load_gradle_version_catalog(&buildsrc_root, &props);
        build_contexts.push(GradleBuildContext {
            project_path_prefix: GRADLE_BUILDSRC_PROJECT_PATH.to_string(),
            build_root: buildsrc_root,
            gradle_properties: props,
            version_catalog: catalog,
        });
    }
    // Deterministic longest-prefix matching.
    build_contexts.sort_by(|a, b| {
        b.project_path_prefix
            .len()
            .cmp(&a.project_path_prefix.len())
            .then(a.project_path_prefix.cmp(&b.project_path_prefix))
    });

    // Root build scripts can define shared dependencies via `subprojects { ... }` / `allprojects { ... }`
    // blocks. Collect these dependencies for *each* independent Gradle build (root, buildSrc, included
    // builds) so the heuristic `ProjectConfig` still sees common deps even when subproject build
    // scripts are empty.
    for ctx in &build_contexts {
        // Direct dependencies in the root project's build script.
        dependencies.extend(parse_gradle_dependencies(
            &ctx.build_root,
            ctx.version_catalog.as_ref(),
            &ctx.gradle_properties,
        ));
        // Dependencies declared in `subprojects {}` / `allprojects {}` blocks.
        let (subprojects, allprojects) = parse_gradle_root_subprojects_allprojects_dependencies(
            &ctx.build_root,
            ctx.version_catalog.as_ref(),
            &ctx.gradle_properties,
        );
        dependencies.extend(subprojects);
        dependencies.extend(allprojects);
    }

    for module_ref in module_refs {
        let project_path = &module_ref.project_path;

        let module_root = if module_ref.dir_rel == "." {
            root.to_path_buf()
        } else if let Some(dir) = snapshot_project_dirs.get(project_path) {
            dir.clone()
        } else {
            root.join(&module_ref.dir_rel)
        };
        let module_root = canonicalize_or_fallback(&module_root);

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

        modules.push(Module {
            name: module_display_name,
            root: module_root.clone(),
            annotation_processing: Default::default(),
        });

        if aggregate_java_across_modules {
            let module_java = parse_gradle_java_config(&module_root).unwrap_or(root_java);
            workspace_java.source = workspace_java.source.max(module_java.source);
            workspace_java.target = workspace_java.target.max(module_java.target);
            workspace_java.enable_preview |= module_java.enable_preview;
        }

        if let Some(cfg) = snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.java_compile_configs.get(project_path))
        {
            for src_root in &cfg.main_source_roots {
                let path = resolve_snapshot_project_dir(root, src_root);
                if !path.as_os_str().is_empty() {
                    let path = canonicalize_or_fallback(&path);
                    source_roots.push(SourceRoot {
                        kind: SourceRootKind::Main,
                        origin: SourceRootOrigin::Source,
                        path,
                    });
                }
            }
            for src_root in &cfg.test_source_roots {
                let path = resolve_snapshot_project_dir(root, src_root);
                if !path.as_os_str().is_empty() {
                    let path = canonicalize_or_fallback(&path);
                    source_roots.push(SourceRoot {
                        kind: SourceRootKind::Test,
                        origin: SourceRootOrigin::Source,
                        path,
                    });
                }
            }
            crate::generated::append_generated_source_roots(
                &mut source_roots,
                root,
                &module_root,
                BuildSystem::Gradle,
                &options.nova_config,
            );

            append_source_set_java_roots(&mut source_roots, &module_root);
            let main_output = cfg
                .main_output_dir
                .as_deref()
                .map(|p| canonicalize_or_fallback(&resolve_snapshot_project_dir(root, p)))
                .unwrap_or_else(|| module_root.join("build/classes/java/main"));
            let test_output = cfg
                .test_output_dir
                .as_deref()
                .map(|p| canonicalize_or_fallback(&resolve_snapshot_project_dir(root, p)))
                .unwrap_or_else(|| module_root.join("build/classes/java/test"));

            output_dirs.push(OutputDir {
                kind: OutputDirKind::Main,
                path: main_output.clone(),
            });
            output_dirs.push(OutputDir {
                kind: OutputDirKind::Test,
                path: test_output.clone(),
            });

            // Ensure output directories appear on the classpath even if the snapshot is partial.
            classpath.push(ClasspathEntry {
                kind: ClasspathEntryKind::Directory,
                path: main_output,
            });
            classpath.push(ClasspathEntry {
                kind: ClasspathEntryKind::Directory,
                path: test_output,
            });

            for entry in cfg
                .compile_classpath
                .iter()
                .chain(cfg.test_classpath.iter())
            {
                let path = resolve_snapshot_project_dir(root, entry);
                if path.as_os_str().is_empty() {
                    continue;
                }
                let path = canonicalize_or_fallback(&path);
                classpath.push(ClasspathEntry {
                    kind: classpath_entry_kind_for_path(&path),
                    path,
                });
            }
            for entry in &cfg.module_path {
                let path = resolve_snapshot_project_dir(root, entry);
                if path.as_os_str().is_empty() {
                    continue;
                }
                let path = canonicalize_or_fallback(&path);
                module_path.push(ClasspathEntry {
                    kind: classpath_entry_kind_for_path(&path),
                    path,
                });
            }
        } else {
            append_source_set_java_roots(&mut source_roots, &module_root);
            crate::generated::append_generated_source_roots(
                &mut source_roots,
                root,
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
        }

        // Dependency extraction is best-effort; useful for later external jar resolution.
        let ctx = build_contexts
            .iter()
            .find(|ctx| project_path.starts_with(&ctx.project_path_prefix))
            .expect("expected gradle project path to match a build context");
        dependencies.extend(parse_gradle_dependencies(
            &module_root,
            ctx.version_catalog.as_ref(),
            &ctx.gradle_properties,
        ));

        // `subprojects { ... }` / `allprojects { ... }` root blocks may reference Gradle properties
        // that are only defined in a subproject's `gradle.properties`. When that happens, re-parse
        // the root blocks using the module's merged properties so we still discover a resolved
        // version in the heuristic dependency list (which helps Gradle cache jar lookup).
        if module_root != ctx.build_root {
            let module_gradle_properties =
                merged_gradle_properties_for_module(&module_root, &ctx.gradle_properties);
            if matches!(&module_gradle_properties, Cow::Owned(_)) {
                let (subprojects, allprojects) =
                    parse_gradle_root_subprojects_allprojects_dependencies(
                        &ctx.build_root,
                        ctx.version_catalog.as_ref(),
                        module_gradle_properties.as_ref(),
                    );
                dependencies.extend(subprojects);
                dependencies.extend(allprojects);
            }
        }

        // Best-effort: add local jars/directories referenced via `files(...)` / `fileTree(...)`.
        // This intentionally does not attempt full Gradle dependency resolution.
        dependency_entries.extend(parse_gradle_local_classpath_entries(&module_root));
    }

    // Sort/dedup before resolving jars so we don't scan the cache repeatedly for
    // the same coordinates.
    sort_dedup_dependencies(&mut dependencies);

    // Best-effort jar discovery for pinned Maven coordinates already present in
    // the Gradle cache (no transitive resolution / variants / etc).
    if let Some(gradle_user_home) = gradle_user_home.as_deref() {
        for dep in &dependencies {
            for jar_path in gradle_dependency_jar_paths(gradle_user_home, dep) {
                dependency_entries.push(ClasspathEntry {
                    kind: ClasspathEntryKind::Jar,
                    path: jar_path,
                });
            }
        }
    }

    sort_dedup_modules(&mut modules, root);

    // Add user-provided classpath entries for unresolved dependencies (Gradle).
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
    sort_dedup_output_dirs(&mut output_dirs);
    sort_dedup_classpath(&mut dependency_entries);
    sort_dedup_classpath(&mut module_path);
    sort_dedup_classpath(&mut classpath);
    // `dependencies` was already sorted/deduped above.

    let jpms_modules = crate::jpms::discover_jpms_modules(&modules);
    let (mut extra_module_path, classpath_deps) =
        crate::jpms::classify_dependency_entries(&jpms_modules, dependency_entries);
    module_path.append(&mut extra_module_path);
    classpath.extend(classpath_deps);
    sort_dedup_classpath(&mut module_path);
    sort_dedup_classpath(&mut classpath);
    let jpms_workspace = crate::jpms::build_jpms_workspace(&jpms_modules, &module_path);

    Ok(ProjectConfig {
        workspace_root: root.to_path_buf(),
        build_system: BuildSystem::Gradle,
        java: workspace_java,
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

    let (mut module_refs, include_builds) = if let Some(settings_path) = settings_path.as_ref() {
        let contents =
            std::fs::read_to_string(settings_path).map_err(|source| ProjectError::Io {
                path: settings_path.clone(),
                source,
            })?;
        (
            parse_gradle_settings_projects(&contents),
            parse_gradle_settings_included_builds(&contents),
        )
    } else {
        (vec![GradleModuleRef::root()], Vec::new())
    };

    // See `load_gradle_project`: include the root project as a module when it contains sources,
    // even if subprojects exist. Keep the root first for deterministic ordering.
    if settings_path.is_some()
        && root_project_has_sources(root)
        && !module_refs.iter().any(|m| m.dir_rel == ".")
    {
        module_refs.insert(0, GradleModuleRef::root());
    }

    let snapshot = load_gradle_snapshot(root);
    maybe_insert_buildsrc_module_ref(&mut module_refs, root, snapshot.as_ref());
    let buildsrc_builds: Vec<GradleModuleRef> = module_refs
        .iter()
        .filter(|m| m.project_path == GRADLE_BUILDSRC_PROJECT_PATH)
        .cloned()
        .collect();
    let included_builds = append_included_build_module_refs(&mut module_refs, root, include_builds);
    append_included_build_subproject_module_refs(&mut module_refs, root, &included_builds);
    append_included_build_subproject_module_refs(&mut module_refs, root, &buildsrc_builds);

    let mut snapshot_project_dirs: HashMap<String, PathBuf> = HashMap::new();
    if let Some(snapshot) = snapshot.as_ref() {
        for project in &snapshot.projects {
            snapshot_project_dirs.insert(
                project.path.clone(),
                resolve_snapshot_project_dir(root, &project.project_dir),
            );
        }
        for (project_path, cfg) in &snapshot.java_compile_configs {
            snapshot_project_dirs
                .entry(project_path.clone())
                .or_insert_with(|| resolve_snapshot_project_dir(root, &cfg.project_dir));
        }
    }

    let (mut root_java, root_java_provenance) = match parse_gradle_java_config_with_path(root) {
        Some((java, path)) => (java, LanguageLevelProvenance::BuildFile(path)),
        None => (JavaConfig::default(), LanguageLevelProvenance::Default),
    };
    let snapshot_java = snapshot.as_ref().and_then(java_config_from_snapshot);
    let aggregate_java_across_modules = snapshot_java.is_none();
    if let Some(java) = snapshot_java {
        root_java = java;
    }
    let mut workspace_java = root_java;

    // Best-effort Gradle cache resolution. This does not execute Gradle; it only
    // adds jars that already exist in the local Gradle cache.
    let gradle_user_home = options
        .gradle_user_home
        .clone()
        .or_else(default_gradle_user_home);
    let gradle_properties = load_gradle_properties(root);
    let version_catalog = load_gradle_version_catalog(root, &gradle_properties);
    let (root_subprojects_deps, root_allprojects_deps) =
        parse_gradle_root_subprojects_allprojects_dependencies(
            root,
            version_catalog.as_ref(),
            &gradle_properties,
        );
    let mut root_common_deps =
        parse_gradle_root_dependencies(root, version_catalog.as_ref(), &gradle_properties);
    retain_dependencies_not_in(&mut root_common_deps, &root_subprojects_deps);
    retain_dependencies_not_in(&mut root_common_deps, &root_allprojects_deps);
    sort_dedup_dependencies(&mut root_common_deps);

    struct IncludedBuildDepsContext {
        project_path_prefix: String,
        build_root: PathBuf,
        gradle_properties: GradleProperties,
        version_catalog: Option<GradleVersionCatalog>,
        root_common_deps: Vec<Dependency>,
        root_subprojects_deps: Vec<Dependency>,
        root_allprojects_deps: Vec<Dependency>,
    }

    let mut included_build_contexts: Vec<IncludedBuildDepsContext> = included_builds
        .iter()
        .map(|included| {
            let build_root = canonicalize_or_fallback(&root.join(&included.dir_rel));
            let props = load_gradle_properties(&build_root);
            let catalog = load_gradle_version_catalog(&build_root, &props);
            let (subprojects, allprojects) = parse_gradle_root_subprojects_allprojects_dependencies(
                &build_root,
                catalog.as_ref(),
                &props,
            );
            let mut common = parse_gradle_root_dependencies(&build_root, catalog.as_ref(), &props);
            retain_dependencies_not_in(&mut common, &subprojects);
            retain_dependencies_not_in(&mut common, &allprojects);
            sort_dedup_dependencies(&mut common);
            IncludedBuildDepsContext {
                project_path_prefix: included.project_path.clone(),
                build_root: build_root.clone(),
                gradle_properties: props,
                version_catalog: catalog,
                root_common_deps: common,
                root_subprojects_deps: subprojects,
                root_allprojects_deps: allprojects,
            }
        })
        .collect();
    if module_refs
        .iter()
        .any(|m| m.project_path == GRADLE_BUILDSRC_PROJECT_PATH)
    {
        let build_root = snapshot_project_dirs
            .get(GRADLE_BUILDSRC_PROJECT_PATH)
            .cloned()
            .unwrap_or_else(|| root.join("buildSrc"));
        let build_root = canonicalize_or_fallback(&build_root);
        let props = load_gradle_properties(&build_root);
        let catalog = load_gradle_version_catalog(&build_root, &props);
        let (subprojects, allprojects) = parse_gradle_root_subprojects_allprojects_dependencies(
            &build_root,
            catalog.as_ref(),
            &props,
        );
        let mut common = parse_gradle_root_dependencies(&build_root, catalog.as_ref(), &props);
        retain_dependencies_not_in(&mut common, &subprojects);
        retain_dependencies_not_in(&mut common, &allprojects);
        sort_dedup_dependencies(&mut common);
        included_build_contexts.push(IncludedBuildDepsContext {
            project_path_prefix: GRADLE_BUILDSRC_PROJECT_PATH.to_string(),
            build_root,
            gradle_properties: props,
            version_catalog: catalog,
            root_common_deps: common,
            root_subprojects_deps: subprojects,
            root_allprojects_deps: allprojects,
        });
    }
    included_build_contexts.sort_by(|a, b| {
        b.project_path_prefix
            .len()
            .cmp(&a.project_path_prefix.len())
            .then(a.project_path_prefix.cmp(&b.project_path_prefix))
    });

    let mut module_configs = Vec::new();
    let mut project_deps: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for module_ref in module_refs {
        let module_root = if module_ref.dir_rel == "." {
            root.to_path_buf()
        } else if let Some(dir) = snapshot_project_dirs.get(&module_ref.project_path) {
            dir.clone()
        } else {
            root.join(&module_ref.dir_rel)
        };
        let module_root = canonicalize_or_fallback(&module_root);

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

        if aggregate_java_across_modules {
            workspace_java.source = workspace_java.source.max(module_java.source);
            workspace_java.target = workspace_java.target.max(module_java.target);
            workspace_java.enable_preview |= module_java.enable_preview;
        }

        let mut language_level = ModuleLanguageLevel {
            level: JavaLanguageLevel::from_java_config(module_java),
            provenance,
        };
        if let Some(cfg) = snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.java_compile_configs.get(&module_ref.project_path))
        {
            let snapshot_level = java_language_level_from_snapshot(cfg);
            if snapshot_level.release.is_some()
                || snapshot_level.source.is_some()
                || snapshot_level.target.is_some()
                || snapshot_level.preview
            {
                language_level.level = snapshot_level;
            }
        }

        let mut source_roots = Vec::new();
        if let Some(cfg) = snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.java_compile_configs.get(&module_ref.project_path))
        {
            for src_root in &cfg.main_source_roots {
                let path = resolve_snapshot_project_dir(root, src_root);
                if path.as_os_str().is_empty() {
                    continue;
                }
                let path = canonicalize_or_fallback(&path);
                source_roots.push(SourceRoot {
                    kind: SourceRootKind::Main,
                    origin: SourceRootOrigin::Source,
                    path,
                });
            }
            for src_root in &cfg.test_source_roots {
                let path = resolve_snapshot_project_dir(root, src_root);
                if path.as_os_str().is_empty() {
                    continue;
                }
                let path = canonicalize_or_fallback(&path);
                source_roots.push(SourceRoot {
                    kind: SourceRootKind::Test,
                    origin: SourceRootOrigin::Source,
                    path,
                });
            }
        }
        append_source_set_java_roots(&mut source_roots, &module_root);
        crate::generated::append_generated_source_roots(
            &mut source_roots,
            root,
            &module_root,
            BuildSystem::Gradle,
            &options.nova_config,
        );

        let (main_output, test_output) = if let Some(cfg) = snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.java_compile_configs.get(&module_ref.project_path))
        {
            let main_output = cfg
                .main_output_dir
                .as_deref()
                .map(|p| canonicalize_or_fallback(&resolve_snapshot_project_dir(root, p)))
                .unwrap_or_else(|| module_root.join("build/classes/java/main"));
            let test_output = cfg
                .test_output_dir
                .as_deref()
                .map(|p| canonicalize_or_fallback(&resolve_snapshot_project_dir(root, p)))
                .unwrap_or_else(|| module_root.join("build/classes/java/test"));
            (main_output, test_output)
        } else {
            (
                module_root.join("build/classes/java/main"),
                module_root.join("build/classes/java/test"),
            )
        };
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

        let mut module_path = Vec::new();
        let mut classpath = vec![
            ClasspathEntry {
                kind: ClasspathEntryKind::Directory,
                path: main_output.clone(),
            },
            ClasspathEntry {
                kind: ClasspathEntryKind::Directory,
                path: test_output.clone(),
            },
        ];

        if let Some(cfg) = snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.java_compile_configs.get(&module_ref.project_path))
        {
            for entry in cfg
                .compile_classpath
                .iter()
                .chain(cfg.test_classpath.iter())
            {
                let path = resolve_snapshot_project_dir(root, entry);
                if path.as_os_str().is_empty() {
                    continue;
                }
                let path = canonicalize_or_fallback(&path);
                classpath.push(ClasspathEntry {
                    kind: classpath_entry_kind_for_path(&path),
                    path,
                });
            }
            for entry in &cfg.module_path {
                let path = resolve_snapshot_project_dir(root, entry);
                if path.as_os_str().is_empty() {
                    continue;
                }
                let path = canonicalize_or_fallback(&path);
                module_path.push(ClasspathEntry {
                    kind: classpath_entry_kind_for_path(&path),
                    path,
                });
            }
        }

        // Best-effort: add local jars/directories referenced via `files(...)` / `fileTree(...)`.
        // This intentionally does not attempt full Gradle dependency resolution.
        classpath.extend(parse_gradle_local_classpath_entries(&module_root));

        // Best-effort: record inter-module `project(":...")` dependencies for later output-dir
        // propagation into module classpaths (see post-processing after all module configs are built).
        let composite_build_prefix = composite_build_root_project_path(&module_ref.project_path);
        let deps = parse_gradle_project_dependencies(&module_root)
            .into_iter()
            .map(|dep_project_path| {
                if let Some(prefix) = composite_build_prefix {
                    // Map `project(":foo")` to `:<compositeBuildPrefix>:foo` so we resolve module
                    // outputs within the composite build (e.g. `buildSrc` or `includeBuild`) instead
                    // of accidentally pointing at the outer Gradle build.
                    if dep_project_path == ":" {
                        prefix.to_string()
                    } else if dep_project_path.starts_with(":__includedBuild_")
                        || dep_project_path.starts_with(GRADLE_BUILDSRC_PROJECT_PATH)
                    {
                        dep_project_path
                    } else {
                        format!("{prefix}{dep_project_path}")
                    }
                } else {
                    dep_project_path
                }
            })
            .collect();
        project_deps.insert(module_ref.project_path.clone(), deps);
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
        let project_path = module_ref.project_path.as_str();
        let mut ctx_root_project_path = ":";
        let mut ctx_build_root: &Path = root;
        let mut ctx_props = &gradle_properties;
        let mut ctx_catalog = version_catalog.as_ref();
        let mut ctx_root_common_deps = &root_common_deps;
        let mut ctx_root_subprojects_deps = &root_subprojects_deps;
        let mut ctx_root_allprojects_deps = &root_allprojects_deps;
        for ctx in &included_build_contexts {
            if project_path.starts_with(&ctx.project_path_prefix) {
                ctx_root_project_path = &ctx.project_path_prefix;
                ctx_build_root = &ctx.build_root;
                ctx_props = &ctx.gradle_properties;
                ctx_catalog = ctx.version_catalog.as_ref();
                ctx_root_common_deps = &ctx.root_common_deps;
                ctx_root_subprojects_deps = &ctx.root_subprojects_deps;
                ctx_root_allprojects_deps = &ctx.root_allprojects_deps;
                break;
            }
        }

        let module_gradle_properties = merged_gradle_properties_for_module(&module_root, ctx_props);
        let (root_common_deps, root_subprojects_deps, root_allprojects_deps) =
            if matches!(&module_gradle_properties, Cow::Borrowed(_)) {
                (
                    Cow::Borrowed(ctx_root_common_deps),
                    Cow::Borrowed(ctx_root_subprojects_deps),
                    Cow::Borrowed(ctx_root_allprojects_deps),
                )
            } else {
                let (subprojects, allprojects) =
                    parse_gradle_root_subprojects_allprojects_dependencies(
                        ctx_build_root,
                        ctx_catalog,
                        module_gradle_properties.as_ref(),
                    );
                let mut common = parse_gradle_root_dependencies(
                    ctx_build_root,
                    ctx_catalog,
                    module_gradle_properties.as_ref(),
                );
                retain_dependencies_not_in(&mut common, &subprojects);
                retain_dependencies_not_in(&mut common, &allprojects);
                sort_dedup_dependencies(&mut common);
                (
                    Cow::Owned(common),
                    Cow::Owned(subprojects),
                    Cow::Owned(allprojects),
                )
            };

        let mut dependencies =
            parse_gradle_dependencies(&module_root, ctx_catalog, module_gradle_properties.as_ref());
        // The root build script often declares dependency blocks under `subprojects { ... }`.
        // Those dependencies should apply to subprojects, not to the root project itself.
        //
        // Since our dependency extraction is regex-based (and doesn't model Gradle's scoping
        // semantics), best-effort filter those `subprojects` dependencies out of the root module
        // for each build context (main build, included builds, buildSrc).
        if project_path == ctx_root_project_path {
            retain_dependencies_not_in(&mut dependencies, root_subprojects_deps.as_ref());
        }
        dependencies.extend(root_common_deps.iter().cloned());
        if project_path != ctx_root_project_path {
            dependencies.extend(root_subprojects_deps.iter().cloned());
        }
        dependencies.extend(root_allprojects_deps.iter().cloned());

        // Sort/dedup before resolving jars so we don't scan the cache repeatedly
        // for the same coordinates.
        sort_dedup_dependencies(&mut dependencies);

        // Best-effort jar discovery for pinned Maven coordinates already present
        // in the Gradle cache (no transitive resolution / variants / etc).
        if let Some(gradle_user_home) = gradle_user_home.as_deref() {
            for dep in &dependencies {
                for jar_path in gradle_dependency_jar_paths(gradle_user_home, dep) {
                    classpath.push(ClasspathEntry {
                        kind: ClasspathEntryKind::Jar,
                        path: jar_path,
                    });
                }
            }
        }

        sort_dedup_source_roots(&mut source_roots);
        sort_dedup_output_dirs(&mut output_dirs);
        sort_dedup_classpath(&mut module_path);
        sort_dedup_classpath(&mut classpath);
        // `dependencies` was already sorted/deduped above.

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
            module_path,
            classpath,
            dependencies,
        });
    }

    sort_dedup_workspace_modules(&mut module_configs);

    // Best-effort: propagate inter-module `project(":...")` dependencies into module classpaths.
    //
    // Gradle's "project dependency" graph is normally handled by Gradle itself. Since Nova does
    // not execute Gradle for heuristic workspace loading, approximate it by wiring dependent
    // module output directories onto the classpath.
    let mut project_outputs: HashMap<String, (PathBuf, PathBuf)> = HashMap::new();
    for module in &module_configs {
        let WorkspaceModuleBuildId::Gradle { project_path } = &module.build_id else {
            continue;
        };

        let mut main = None;
        let mut test = None;
        for dir in &module.output_dirs {
            match dir.kind {
                OutputDirKind::Main => main = Some(dir.path.clone()),
                OutputDirKind::Test => test = Some(dir.path.clone()),
            }
        }
        let (Some(main), Some(test)) = (main, test) else {
            continue;
        };
        project_outputs.insert(project_path.clone(), (main, test));
    }

    for module in &mut module_configs {
        let WorkspaceModuleBuildId::Gradle { project_path } = &module.build_id else {
            continue;
        };

        let deps = transitive_gradle_project_dependencies(project_path, &project_deps);
        for dep_project_path in deps {
            if dep_project_path == *project_path {
                continue;
            }
            let Some((main_output, _test_output)) = project_outputs.get(&dep_project_path) else {
                continue;
            };
            module.classpath.push(ClasspathEntry {
                kind: ClasspathEntryKind::Directory,
                path: main_output.clone(),
            });
        }

        sort_dedup_classpath(&mut module.classpath);
    }

    let modules_for_jpms = module_configs
        .iter()
        .map(|module| Module {
            name: module.name.clone(),
            root: module.root.clone(),
            annotation_processing: Default::default(),
        })
        .collect::<Vec<_>>();
    let jpms_modules = crate::jpms::discover_jpms_modules(&modules_for_jpms);

    // JPMS-aware workspace model: when the workspace contains any `module-info.java`, classify
    // dependency entries into module-path vs classpath. Keep known output directories on the
    // classpath.
    let mut override_entries = Vec::new();
    for entry in &options.classpath_overrides {
        override_entries.push(ClasspathEntry {
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
    sort_dedup_classpath(&mut override_entries);

    for module in &mut module_configs {
        let output_dir_paths: BTreeSet<_> =
            module.output_dirs.iter().map(|o| o.path.clone()).collect();

        let mut output_entries = Vec::new();
        let mut dependency_entries = override_entries.clone();

        for entry in std::mem::take(&mut module.classpath) {
            let is_output_dir = entry.kind == ClasspathEntryKind::Directory
                && output_dir_paths.contains(&entry.path);
            if is_output_dir {
                output_entries.push(entry);
            } else {
                dependency_entries.push(entry);
            }
        }

        sort_dedup_classpath(&mut dependency_entries);
        let (mut module_path_deps, classpath_deps) =
            crate::jpms::classify_dependency_entries(&jpms_modules, dependency_entries);
        module.module_path.append(&mut module_path_deps);
        module.classpath = output_entries;
        module.classpath.extend(classpath_deps);

        sort_dedup_classpath(&mut module.module_path);
        sort_dedup_classpath(&mut module.classpath);
    }

    // JPMS-aware workspace model: if the workspace contains any `module-info.java`, treat jar
    // dependencies as module-path entries so downstream consumers (e.g. `nova-db`) can build a
    // module-aware classpath index.
    //
    // Keep output directories (class dirs) in `classpath`.
    if crate::jpms::workspace_uses_jpms(&jpms_modules) {
        for module in &mut module_configs {
            let mut module_path = std::mem::take(&mut module.module_path);
            let mut classpath = Vec::new();

            for entry in std::mem::take(&mut module.classpath) {
                match entry.kind {
                    ClasspathEntryKind::Jar => module_path.push(entry),
                    ClasspathEntryKind::Directory => classpath.push(entry),
                }
            }

            sort_dedup_classpath(&mut module_path);
            sort_dedup_classpath(&mut classpath);

            module.module_path = module_path;
            module.classpath = classpath;
        }
    }

    Ok(WorkspaceProjectModel::new(
        root.to_path_buf(),
        BuildSystem::Gradle,
        workspace_java,
        module_configs,
        jpms_modules,
    ))
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

fn sanitize_included_build_name(name: &str) -> String {
    // Gradle project paths use `:` as a separator. We synthesize a single-segment project name
    // based on the included build directory name, replacing any characters outside of a small
    // safe set.
    let name = name.trim();
    if name.is_empty() {
        return "includedBuild".to_string();
    }

    let mut out = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }

    if out.chars().all(|c| c == '_') {
        "includedBuild".to_string()
    } else {
        out
    }
}

fn composite_build_root_project_path(project_path: &str) -> Option<&str> {
    // Gradle's `buildSrc` is a separate build. We model it via a stable synthetic project path
    // `:__buildSrc` and may also synthesize nested project paths like `:__buildSrc:subproject` when
    // `buildSrc/settings.gradle(.kts)` is present.
    if let Some(rest) = project_path.strip_prefix(GRADLE_BUILDSRC_PROJECT_PATH) {
        if rest.is_empty() || rest.starts_with(':') {
            return Some(GRADLE_BUILDSRC_PROJECT_PATH);
        }
    }

    if !project_path.starts_with(":__includedBuild_") {
        return None;
    }

    // Included build project paths are synthesized as:
    // - `:__includedBuild_<name>` for the included build root, and
    // - `:__includedBuild_<name>:subproject` for included build subprojects.
    //
    // Extract the `:__includedBuild_<name>` prefix so we can scope `project(":...")` dependencies
    // within the included build.
    let rest = project_path.strip_prefix(':').unwrap_or(project_path);
    match rest.find(':') {
        Some(idx) => Some(&project_path[..idx + 1]),
        None => Some(project_path),
    }
}

fn append_included_build_module_refs(
    module_refs: &mut Vec<GradleModuleRef>,
    workspace_root: &Path,
    dirs: Vec<String>,
) -> Vec<GradleModuleRef> {
    if dirs.is_empty() {
        return Vec::new();
    }

    let mut used_project_paths: BTreeSet<String> =
        module_refs.iter().map(|m| m.project_path.clone()).collect();
    let mut existing_dirs: BTreeSet<String> =
        module_refs.iter().map(|m| m.dir_rel.clone()).collect();
    // Best-effort canonicalization to deduplicate includeBuild roots that refer to the same
    // directory via different relative paths (e.g. `../included` vs `../included/.`).
    //
    // This also avoids generating unstable synthetic project paths when the includeBuild path ends
    // in `/.` (which would otherwise produce a base name of `"."`).
    let mut existing_build_roots: BTreeSet<PathBuf> = module_refs
        .iter()
        .filter_map(|m| std::fs::canonicalize(workspace_root.join(&m.dir_rel)).ok())
        .collect();

    let mut added = Vec::new();
    for dir_rel in dirs {
        if existing_dirs.contains(&dir_rel) {
            continue;
        }

        let build_root = workspace_root.join(&dir_rel);
        if !build_root.is_dir() || !is_gradle_marker_root(&build_root) {
            continue;
        }

        let canonical_build_root = std::fs::canonicalize(&build_root).unwrap_or(build_root);
        if existing_build_roots.contains(&canonical_build_root) {
            continue;
        }
        existing_build_roots.insert(canonical_build_root.clone());

        let base_name = canonical_build_root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&dir_rel);
        let base_name = sanitize_included_build_name(base_name);

        let base_project_path = format!(":__includedBuild_{base_name}");
        let mut project_path = base_project_path.clone();
        let mut suffix = 2usize;
        while used_project_paths.contains(&project_path) {
            project_path = format!("{base_project_path}_{suffix}");
            suffix += 1;
        }
        used_project_paths.insert(project_path.clone());
        existing_dirs.insert(dir_rel.clone());

        let module_ref = GradleModuleRef {
            project_path,
            dir_rel,
        };
        module_refs.push(module_ref.clone());
        added.push(module_ref);
    }

    added
}

fn append_included_build_subproject_module_refs(
    module_refs: &mut Vec<GradleModuleRef>,
    workspace_root: &Path,
    included_builds: &[GradleModuleRef],
) {
    if included_builds.is_empty() {
        return;
    }

    let mut used_project_paths: BTreeSet<String> =
        module_refs.iter().map(|m| m.project_path.clone()).collect();
    let mut existing_dirs: BTreeSet<String> =
        module_refs.iter().map(|m| m.dir_rel.clone()).collect();

    for included_build in included_builds {
        let build_root = workspace_root.join(&included_build.dir_rel);
        let settings_path = ["settings.gradle.kts", "settings.gradle"]
            .into_iter()
            .map(|name| build_root.join(name))
            .find(|path| path.is_file());
        let Some(settings_path) = settings_path else {
            continue;
        };
        let Ok(contents) = std::fs::read_to_string(&settings_path) else {
            continue;
        };

        for subproject in parse_gradle_settings_projects(&contents) {
            // `append_included_build_module_refs` already adds the included build root module,
            // so only append subprojects.
            if subproject.project_path == ":" {
                continue;
            }

            let project_path =
                format!("{}{}", included_build.project_path, subproject.project_path);
            if used_project_paths.contains(&project_path) {
                continue;
            }

            let combined_dir_rel = if subproject.dir_rel == "." {
                included_build.dir_rel.clone()
            } else {
                format!(
                    "{}/{}",
                    included_build.dir_rel.trim_end_matches('/'),
                    subproject.dir_rel
                )
            };
            let Some(dir_rel) = normalize_dir_rel(&combined_dir_rel) else {
                continue;
            };
            if existing_dirs.contains(&dir_rel) {
                continue;
            }

            used_project_paths.insert(project_path.clone());
            existing_dirs.insert(dir_rel.clone());
            module_refs.push(GradleModuleRef {
                project_path,
                dir_rel,
            });
        }
    }
}

fn parse_gradle_settings_projects(contents: &str) -> Vec<GradleModuleRef> {
    let contents = strip_gradle_comments(contents);

    let mut included = parse_gradle_settings_included_projects(&contents);
    let include_flat_dirs = parse_gradle_settings_include_flat_project_dirs(&contents);
    included.extend(include_flat_dirs.keys().cloned());

    if included.is_empty() {
        return vec![GradleModuleRef::root()];
    }

    let overrides = parse_gradle_settings_project_dir_overrides(&contents);

    // Deterministic + dedup: module refs are sorted by Gradle project path.
    let included: BTreeSet<_> = included.into_iter().collect();

    included
        .into_iter()
        .map(|project_path| {
            let dir_rel = overrides
                .get(&project_path)
                .cloned()
                .or_else(|| include_flat_dirs.get(&project_path).cloned())
                .unwrap_or_else(|| heuristic_dir_rel_for_project_path(&project_path));

            GradleModuleRef {
                project_path,
                dir_rel,
            }
        })
        .collect()
}

fn extract_quoted_strings(text: &str) -> Vec<String> {
    // Best-effort string literal extraction for Gradle settings parsing.
    //
    // Supports:
    // - `'...'`
    // - `"..."` (with backslash escapes)
    // - `'''...'''` / `"""..."""` (Groovy / Kotlin raw strings)
    //
    // Note: we intentionally do not unescape contents; callers normalize/trim as needed.
    let bytes = text.as_bytes();
    let mut out = Vec::new();

    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"'''") {
            let start = i + 3;
            i = start;
            while i < bytes.len() && !bytes[i..].starts_with(b"'''") {
                i += 1;
            }
            if i < bytes.len() {
                if start < i {
                    out.push(text[start..i].to_string());
                }
                i += 3;
            }
            continue;
        }

        if bytes[i..].starts_with(b"\"\"\"") {
            let start = i + 3;
            i = start;
            while i < bytes.len() && !bytes[i..].starts_with(b"\"\"\"") {
                i += 1;
            }
            if i < bytes.len() {
                if start < i {
                    out.push(text[start..i].to_string());
                }
                i += 3;
            }
            continue;
        }

        if bytes[i] == b'\'' {
            let start = i + 1;
            i = start;
            while i < bytes.len() {
                let b = bytes[i];
                if b == b'\\' {
                    i = (i + 2).min(bytes.len());
                    continue;
                }
                if b == b'\'' {
                    if start < i {
                        out.push(text[start..i].to_string());
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }

        if bytes[i] == b'"' {
            let start = i + 1;
            i = start;
            while i < bytes.len() {
                let b = bytes[i];
                if b == b'\\' {
                    i = (i + 2).min(bytes.len());
                    continue;
                }
                if b == b'"' {
                    if start < i {
                        out.push(text[start..i].to_string());
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }

        i += 1;
    }

    out
}

fn gradle_string_literal_ranges(contents: &str) -> Vec<Range<usize>> {
    nova_build_model::groovy_scan::gradle_string_literal_ranges(contents)
}

fn normalize_project_path(project_path: &str) -> String {
    let project_path = project_path.trim();
    if project_path.is_empty() || project_path == ":" {
        return ":".to_string();
    }
    if project_path.starts_with(':') {
        project_path.to_string()
    } else {
        format!(":{project_path}")
    }
}

fn heuristic_dir_rel_for_project_path(project_path: &str) -> String {
    let dir_rel = project_path.trim_start_matches(':').replace(':', "/");
    if dir_rel.trim().is_empty() {
        ".".to_string()
    } else {
        dir_rel
    }
}

fn find_keyword_outside_strings(contents: &str, keyword: &str) -> Vec<usize> {
    nova_build_model::groovy_scan::find_keyword_outside_strings(contents, keyword)
}

fn normalize_dir_rel(dir_rel: &str) -> Option<String> {
    let mut dir_rel = dir_rel.trim().replace('\\', "/");
    while let Some(stripped) = dir_rel.strip_prefix("./") {
        dir_rel = stripped.to_string();
    }
    while dir_rel.ends_with('/') {
        dir_rel.pop();
    }

    if dir_rel.is_empty() {
        return Some(".".to_string());
    }

    // Avoid accidentally escaping the workspace root by joining with an absolute path.
    let is_absolute_unix = dir_rel.starts_with('/');
    let is_windows_drive = dir_rel.as_bytes().get(1).is_some_and(|b| *b == b':')
        && dir_rel
            .as_bytes()
            .first()
            .is_some_and(|b| b.is_ascii_alphabetic());
    if is_absolute_unix || is_windows_drive {
        return None;
    }

    Some(dir_rel)
}

fn parse_gradle_settings_included_projects(contents: &str) -> Vec<String> {
    let mut projects = Vec::new();
    for start in find_keyword_outside_strings(contents, "include") {
        let mut idx = start + "include".len();
        let bytes = contents.as_bytes();
        while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
            idx += 1;
        }
        if idx >= bytes.len() {
            continue;
        }

        let args = if bytes[idx] == b'(' {
            extract_balanced_parens(contents, idx)
                .map(|(args, _end)| args)
                .unwrap_or_default()
        } else {
            extract_unparenthesized_args_until_eol_or_continuation(contents, idx)
        };

        projects.extend(
            extract_quoted_strings(&args)
                .into_iter()
                .map(|s| normalize_project_path(&s)),
        );
    }

    projects
}

pub(crate) fn parse_gradle_settings_included_builds(contents: &str) -> Vec<String> {
    // Keep `includeBuild(...)` parsing in sync with Gradle build-file fingerprinting.
    nova_build_model::parse_gradle_settings_included_builds(contents)
}

fn parse_gradle_settings_include_flat_project_dirs(contents: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();

    for start in find_keyword_outside_strings(contents, "includeFlat") {
        let mut idx = start + "includeFlat".len();
        let bytes = contents.as_bytes();
        while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
            idx += 1;
        }
        if idx >= bytes.len() {
            continue;
        }

        let args = if bytes[idx] == b'(' {
            extract_balanced_parens(contents, idx)
                .map(|(args, _end)| args)
                .unwrap_or_default()
        } else {
            extract_unparenthesized_args_until_eol_or_continuation(contents, idx)
        };

        for raw in extract_quoted_strings(&args) {
            let project_path = normalize_project_path(&raw);
            let name = raw.trim().trim_start_matches(':').replace([':', '\\'], "/");
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            let dir_rel = format!("../{name}");
            let Some(dir_rel) = normalize_dir_rel(&dir_rel) else {
                continue;
            };
            out.insert(project_path, dir_rel);
        }
    }

    out
}

fn extract_balanced_parens(contents: &str, open_paren_index: usize) -> Option<(String, usize)> {
    nova_build_model::groovy_scan::extract_balanced_parens(contents, open_paren_index)
}

fn extract_balanced_braces(contents: &str, open_brace_index: usize) -> Option<(String, usize)> {
    nova_build_model::groovy_scan::extract_balanced_braces(contents, open_brace_index)
}

fn find_keyword_positions_outside_strings(contents: &str, keyword: &str) -> Vec<usize> {
    nova_build_model::groovy_scan::find_keyword_positions_outside_strings(contents, keyword)
}

fn extract_named_brace_blocks_from_stripped(stripped: &str, keyword: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = stripped.as_bytes();

    for start in find_keyword_positions_outside_strings(stripped, keyword) {
        let mut idx = start + keyword.len();
        while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
            idx += 1;
        }
        if idx >= bytes.len() {
            continue;
        }

        // Handle `keyword(...) { ... }` form by skipping a single balanced `(...)` argument list.
        if bytes[idx] == b'(' {
            if let Some((_args, end)) = extract_balanced_parens(stripped, idx) {
                idx = end;
                while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
                    idx += 1;
                }
            }
        }

        if idx >= bytes.len() || bytes[idx] != b'{' {
            continue;
        }

        if let Some((body, _end)) = extract_balanced_braces(stripped, idx) {
            out.push(body);
        }
    }

    out
}

fn extract_named_brace_blocks(contents: &str, keyword: &str) -> Vec<String> {
    let stripped = strip_gradle_comments(contents);
    extract_named_brace_blocks_from_stripped(&stripped, keyword)
}

fn extract_unparenthesized_args_until_eol_or_continuation(contents: &str, start: usize) -> String {
    nova_build_model::groovy_scan::extract_unparenthesized_args_until_eol_or_continuation(
        contents, start,
    )
}

fn parse_gradle_settings_project_dir_overrides(contents: &str) -> BTreeMap<String, String> {
    // Common overrides:
    //   project(':app').projectDir = file('modules/app')
    //   project(':lib').projectDir = new File(settingsDir, 'modules/lib')
    //   project(":app").projectDir = file("modules/app") (Kotlin DSL)
    let mut overrides = BTreeMap::new();
    let bytes = contents.as_bytes();

    for start in find_keyword_outside_strings(contents, "project") {
        let mut idx = start + "project".len();
        while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
            idx += 1;
        }
        if bytes.get(idx) != Some(&b'(') {
            continue;
        }

        let Some((project_args, after_project_parens)) = extract_balanced_parens(contents, idx)
        else {
            continue;
        };
        let Some(project_path) = extract_quoted_strings(&project_args).into_iter().next() else {
            continue;
        };
        let project_path = normalize_project_path(&project_path);

        // Parse:
        //   project(...).projectDir = ...
        //              ^^^^^^^^^^
        let mut cursor = after_project_parens;
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if bytes.get(cursor) != Some(&b'.') {
            continue;
        }
        cursor += 1;
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if !bytes[cursor..].starts_with(b"projectDir") {
            continue;
        }
        cursor += "projectDir".len();
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if bytes.get(cursor) != Some(&b'=') {
            continue;
        }
        cursor += 1;
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }

        // Parse RHS:
        // - file("modules/app")
        // - new File(settingsDir, "modules/app")
        // - java.io.File(settingsDir, "modules/app")
        let dir = if bytes
            .get(cursor..)
            .is_some_and(|rest| rest.starts_with(b"file"))
        {
            cursor += "file".len();
            while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            if bytes.get(cursor) != Some(&b'(') {
                continue;
            }
            let Some((args, _end)) = extract_balanced_parens(contents, cursor) else {
                continue;
            };
            extract_quoted_strings(&args).into_iter().next()
        } else {
            // Optional `new`.
            if bytes
                .get(cursor..)
                .is_some_and(|rest| rest.starts_with(b"new"))
            {
                cursor += "new".len();
                while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                    cursor += 1;
                }
            }

            // Optional `java.io.` prefix.
            if bytes
                .get(cursor..)
                .is_some_and(|rest| rest.starts_with(b"java.io."))
            {
                cursor += "java.io.".len();
            }

            if !bytes
                .get(cursor..)
                .is_some_and(|rest| rest.starts_with(b"File"))
            {
                continue;
            }
            cursor += "File".len();
            while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            if bytes.get(cursor) != Some(&b'(') {
                continue;
            }
            let Some((args, _end)) = extract_balanced_parens(contents, cursor) else {
                continue;
            };
            // Best-effort: accept `File(settingsDir, "...")` and `File(rootDir, "...")`.
            let args_trim = args.trim_start();
            if !(args_trim.starts_with("settingsDir") || args_trim.starts_with("rootDir")) {
                continue;
            }
            extract_quoted_strings(&args).into_iter().next()
        };

        let Some(dir) = dir.as_deref().map(str::trim).filter(|d| !d.is_empty()) else {
            continue;
        };
        let Some(dir_rel) = normalize_dir_rel(dir) else {
            continue;
        };
        overrides.insert(project_path, dir_rel);
    }
    overrides
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
    // Precedence rules (deterministic):
    // 1) Prefer explicit `sourceCompatibility` / `targetCompatibility` assignments when present.
    // 2) Otherwise, fall back to Gradle toolchains (`JavaLanguageVersion.of(N)`).
    // 3) Otherwise, return `None` (caller uses defaults), unless we can still infer flags like
    //    `--enable-preview`.
    let contents = strip_gradle_comments(contents);
    let enable_preview = contents.contains("--enable-preview");

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
            enable_preview,
        }),
        (Some(v), None) | (None, Some(v)) => Some(JavaConfig {
            source: v,
            target: v,
            enable_preview,
        }),
        (None, None) => {
            if let Some(v) = parse_java_toolchain_language_version(&contents) {
                Some(JavaConfig {
                    source: v,
                    target: v,
                    enable_preview,
                })
            } else if enable_preview {
                Some(JavaConfig {
                    source: JavaVersion::JAVA_17,
                    target: JavaVersion::JAVA_17,
                    enable_preview,
                })
            } else {
                None
            }
        }
    }
}

fn parse_java_toolchain_language_version(contents: &str) -> Option<JavaVersion> {
    // Best-effort text parsing: match both Groovy DSL:
    //   languageVersion = JavaLanguageVersion.of(21)
    // and Kotlin DSL:
    //   languageVersion.set(JavaLanguageVersion.of(21))
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r#"JavaLanguageVersion\s*\.\s*of\s*\(\s*(\d+)\s*\)"#).expect("valid regex")
    });
    let caps = re.captures(contents)?;
    let version = caps.get(1)?.as_str();
    JavaVersion::parse(version)
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

const GRADLE_DEPENDENCY_CONFIGS: &str = r"(?:implementation|api|compile|runtime|compileOnly|compileOnlyApi|provided|providedCompile|runtimeOnly|providedRuntime|testImplementation|testCompile|testRuntime|testRuntimeOnly|testCompileOnly|annotationProcessor|testAnnotationProcessor|apt|testApt|kapt|kaptTest|ksp|kspTest)";

const GRADLE_DEPENDENCY_WRAPPER_PREFIX_RE: &str =
    r"(?:[A-Za-z_][A-Za-z0-9_]*\s*\(\s*|(?:platform|enforcedPlatform)\s+)*";

type GradleProperties = HashMap<String, String>;

fn load_gradle_properties(workspace_root: &Path) -> GradleProperties {
    let path = workspace_root.join("gradle.properties");
    let Ok(contents) = std::fs::read_to_string(path) else {
        return GradleProperties::new();
    };
    parse_gradle_properties_from_text(&contents)
}

fn load_gradle_module_properties(module_root: &Path) -> GradleProperties {
    let path = module_root.join("gradle.properties");
    let Ok(contents) = std::fs::read_to_string(path) else {
        return GradleProperties::new();
    };
    parse_gradle_properties_from_text(&contents)
}

fn merged_gradle_properties_for_module<'a>(
    module_root: &Path,
    root_properties: &'a GradleProperties,
) -> Cow<'a, GradleProperties> {
    let module_properties = load_gradle_module_properties(module_root);
    if module_properties.is_empty() {
        return Cow::Borrowed(root_properties);
    }

    // Deterministic merge: module values are used as a fallback, but root values take precedence.
    let mut merged = module_properties;
    for (k, v) in root_properties {
        merged.insert(k.clone(), v.clone());
    }
    Cow::Owned(merged)
}

/// Best-effort parsing for Gradle's `gradle.properties`.
///
/// This intentionally does **not** implement full Java-properties escaping semantics; it only
/// supports the common `key=value` form and ignores blank lines and comments starting with `#` or
/// `!`.
fn parse_gradle_properties_from_text(contents: &str) -> GradleProperties {
    let mut out = GradleProperties::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        out.insert(key.to_string(), value.trim().to_string());
    }
    out
}

/// Resolves a Gradle/Kotlin `$var` / `${var}` placeholder **only** when the entire string is the
/// placeholder.
///
/// If the property isn't present in `gradle.properties`, this returns `None` and the caller should
/// keep the original string intact.
fn resolve_gradle_properties_placeholder(
    value: &str,
    gradle_properties: &GradleProperties,
) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }

    let key = if let Some(rest) = value.strip_prefix("${") {
        rest.strip_suffix('}')
    } else {
        value.strip_prefix('$')
    }?;

    let key = key.trim();
    if key.is_empty() {
        return None;
    }

    gradle_properties.get(key).cloned()
}

/// Maps Gradle dependency configurations into a small stable set of scopes.
///
/// This is best-effort extraction and intentionally collapses many Gradle configurations into
/// coarse scopes:
/// - `implementation|api|compile` => `compile`
/// - `runtimeOnly|runtime|providedRuntime` => `runtime`
/// - `compileOnly|compileOnlyApi|provided|providedCompile` => `provided`
/// - `testImplementation|testRuntimeOnly|testCompileOnly|testCompile|testRuntime` => `test`
/// - `annotationProcessor|testAnnotationProcessor|apt|testApt|kapt|kaptTest|ksp|kspTest` => `annotationProcessor`
fn gradle_scope_from_configuration(configuration: &str) -> Option<&'static str> {
    let configuration = configuration.trim();
    if configuration.eq_ignore_ascii_case("implementation")
        || configuration.eq_ignore_ascii_case("api")
        || configuration.eq_ignore_ascii_case("compile")
    {
        return Some("compile");
    }

    if configuration.eq_ignore_ascii_case("runtimeOnly")
        || configuration.eq_ignore_ascii_case("runtime")
        || configuration.eq_ignore_ascii_case("providedRuntime")
    {
        return Some("runtime");
    }

    if configuration.eq_ignore_ascii_case("compileOnly")
        || configuration.eq_ignore_ascii_case("compileOnlyApi")
        || configuration.eq_ignore_ascii_case("provided")
        || configuration.eq_ignore_ascii_case("providedCompile")
    {
        return Some("provided");
    }

    if configuration.eq_ignore_ascii_case("testImplementation")
        || configuration.eq_ignore_ascii_case("testRuntimeOnly")
        || configuration.eq_ignore_ascii_case("testCompileOnly")
        || configuration.eq_ignore_ascii_case("testCompile")
        || configuration.eq_ignore_ascii_case("testRuntime")
    {
        return Some("test");
    }

    if configuration.eq_ignore_ascii_case("annotationProcessor")
        || configuration.eq_ignore_ascii_case("testAnnotationProcessor")
        || configuration.eq_ignore_ascii_case("apt")
        || configuration.eq_ignore_ascii_case("testApt")
        || configuration.eq_ignore_ascii_case("kapt")
        || configuration.eq_ignore_ascii_case("kaptTest")
        || configuration.eq_ignore_ascii_case("ksp")
        || configuration.eq_ignore_ascii_case("kspTest")
    {
        return Some("annotationProcessor");
    }

    None
}

#[derive(Debug, Clone, Default)]
struct GradleVersionCatalog {
    versions: HashMap<String, String>,
    libraries: HashMap<String, GradleVersionCatalogLibrary>,
    bundles: HashMap<String, Vec<String>>,
}

#[derive(Debug, Clone)]
struct GradleVersionCatalogLibrary {
    group_id: String,
    artifact_id: String,
    version: Option<String>,
}

fn load_gradle_version_catalog(
    workspace_root: &Path,
    gradle_properties: &GradleProperties,
) -> Option<GradleVersionCatalog> {
    // Gradle's conventional default is `gradle/libs.versions.toml`, but some projects (and older
    // Nova fixtures) keep it at the workspace root.
    let candidates = [
        workspace_root.join("gradle").join("libs.versions.toml"),
        workspace_root.join("libs.versions.toml"),
    ];
    let contents = candidates
        .iter()
        .find_map(|path| std::fs::read_to_string(path).ok())?;
    parse_gradle_version_catalog_from_toml(&contents, gradle_properties)
}

fn parse_gradle_version_catalog_from_toml(
    contents: &str,
    gradle_properties: &GradleProperties,
) -> Option<GradleVersionCatalog> {
    let root: Value = toml::from_str(contents).ok()?;
    let root = root.as_table()?;

    let mut catalog = GradleVersionCatalog::default();

    if let Some(versions) = root.get("versions").and_then(Value::as_table) {
        for (k, v) in versions {
            if let Some(v) = v.as_str() {
                let resolved = resolve_gradle_properties_placeholder(v, gradle_properties)
                    .unwrap_or_else(|| v.trim().to_string());
                catalog.versions.insert(k.to_string(), resolved);
            }
        }
    }

    if let Some(libraries) = root.get("libraries").and_then(Value::as_table) {
        for (alias, value) in libraries {
            if let Some(lib) =
                parse_gradle_version_catalog_library(value, &catalog.versions, gradle_properties)
            {
                catalog.libraries.insert(alias.to_string(), lib);
            }
        }
    }

    if let Some(bundles) = root.get("bundles").and_then(Value::as_table) {
        for (bundle_alias, value) in bundles {
            let Some(items) = value.as_array() else {
                continue;
            };
            let libs = items
                .iter()
                .filter_map(Value::as_str)
                .map(|s| s.to_string())
                .collect::<Vec<_>>();
            if !libs.is_empty() {
                catalog.bundles.insert(bundle_alias.to_string(), libs);
            }
        }
    }

    Some(catalog)
}

fn parse_gradle_version_catalog_library(
    value: &Value,
    versions: &HashMap<String, String>,
    gradle_properties: &GradleProperties,
) -> Option<GradleVersionCatalogLibrary> {
    match value {
        // Not part of the requirements, but cheap to support.
        Value::String(text) => {
            let (group_id, artifact_id, version) = parse_maybe_maven_coord(text)?;
            let version = resolve_gradle_properties_placeholder(&version, gradle_properties)
                .unwrap_or(version);
            Some(GradleVersionCatalogLibrary {
                group_id,
                artifact_id,
                version: Some(version),
            })
        }
        Value::Table(table) => {
            let (group_id, artifact_id) =
                if let Some(module) = table.get("module").and_then(Value::as_str) {
                    let (group_id, artifact_id) = module.split_once(':')?;
                    (group_id.to_string(), artifact_id.to_string())
                } else {
                    let group_id = table.get("group").and_then(Value::as_str)?;
                    let artifact_id = table.get("name").and_then(Value::as_str)?;
                    (group_id.to_string(), artifact_id.to_string())
                };

            let version = match table.get("version") {
                Some(Value::String(v)) => Some(
                    resolve_gradle_properties_placeholder(v, gradle_properties)
                        .unwrap_or_else(|| v.to_string()),
                ),
                Some(Value::Table(version_table)) => {
                    if let Some(alias) = version_table.get("ref").and_then(Value::as_str) {
                        versions.get(alias).cloned()
                    } else if let Some(v) = version_table.get("strictly").and_then(Value::as_str) {
                        Some(
                            resolve_gradle_properties_placeholder(v, gradle_properties)
                                .unwrap_or_else(|| v.to_string()),
                        )
                    } else if let Some(v) = version_table.get("require").and_then(Value::as_str) {
                        Some(
                            resolve_gradle_properties_placeholder(v, gradle_properties)
                                .unwrap_or_else(|| v.to_string()),
                        )
                    } else {
                        version_table
                            .get("prefer")
                            .and_then(Value::as_str)
                            .map(|v| {
                                resolve_gradle_properties_placeholder(v, gradle_properties)
                                    .unwrap_or_else(|| v.to_string())
                            })
                    }
                }
                _ => None,
            };

            Some(GradleVersionCatalogLibrary {
                group_id,
                artifact_id,
                version,
            })
        }
        _ => None,
    }
}

fn parse_maybe_maven_coord(text: &str) -> Option<(String, String, String)> {
    let text = text.trim();
    let mut parts = text.split(':');
    let group_id = parts.next()?.trim().to_string();
    let artifact_id = parts.next()?.trim().to_string();
    let version = parts.next()?.trim().to_string();
    if group_id.is_empty() || artifact_id.is_empty() || version.is_empty() {
        return None;
    }
    Some((group_id, artifact_id, version))
}

fn parse_gradle_dependencies(
    module_root: &Path,
    version_catalog: Option<&GradleVersionCatalog>,
    gradle_properties: &GradleProperties,
) -> Vec<Dependency> {
    let gradle_properties = merged_gradle_properties_for_module(module_root, gradle_properties);
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

        out.extend(parse_gradle_dependencies_from_text(
            &contents,
            version_catalog,
            gradle_properties.as_ref(),
        ));
    }
    out
}

fn parse_gradle_root_dependencies(
    root: &Path,
    version_catalog: Option<&GradleVersionCatalog>,
    gradle_properties: &GradleProperties,
) -> Vec<Dependency> {
    // Root build scripts in multi-project Gradle repos often declare shared dependencies via
    // `allprojects { dependencies { ... } }` or `subprojects { dependencies { ... } }`.
    //
    // Parse them separately so we still discover dependencies even when subproject build scripts
    // are minimal.
    let mut deps = parse_gradle_dependencies(root, version_catalog, gradle_properties);
    let (subprojects, allprojects) = parse_gradle_root_subprojects_allprojects_dependencies(
        root,
        version_catalog,
        gradle_properties,
    );
    deps.extend(subprojects);
    deps.extend(allprojects);
    sort_dedup_dependencies(&mut deps);
    deps
}

/// Best-effort extraction of inter-module `project(":...")` dependencies from Gradle build scripts.
///
/// This is intended to cover common forms:
/// - Groovy DSL: `implementation project(":lib")`
/// - Kotlin DSL: `implementation(project(":lib"))`
/// - Groovy DSL: `implementation project(path: ':lib')`
///
/// This does **not** attempt to resolve configurations, dependency constraints, or apply logic from
/// `subprojects { ... }` blocks. It is only used to wire workspace-module output directories into
/// the `WorkspaceProjectModel` classpath as a heuristic approximation of module dependencies.
fn parse_gradle_project_dependencies(module_root: &Path) -> Vec<String> {
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
        let contents = strip_gradle_comments(&contents);
        out.extend(parse_gradle_project_dependencies_from_text(&contents));
    }

    out.sort();
    out.dedup();
    out
}

fn parse_gradle_project_dependencies_from_text(contents: &str) -> Vec<String> {
    static RE_PARENS: OnceLock<Regex> = OnceLock::new();
    static RE_NO_PARENS: OnceLock<Regex> = OnceLock::new();

    let re_parens = RE_PARENS.get_or_init(|| {
        // Keep this list conservative: only configurations that are commonly used for
        // Java compilation or annotation processing.
        let configs = GRADLE_DEPENDENCY_CONFIGS;
        Regex::new(&format!(
            r#"(?i)\b{configs}\b\s*\(?\s*project\s*\(\s*(?:path\s*[:=]\s*)?['"]([^'"]+)['"][^)]*\)\s*\)?"#,
        ))
        .expect("valid regex")
    });

    let re_no_parens = RE_NO_PARENS.get_or_init(|| {
        let configs = GRADLE_DEPENDENCY_CONFIGS;
        Regex::new(&format!(
            r#"(?i)\b{configs}\b\s*\(?\s*project\s+(?:path\s*[:=]\s*)?['"]([^'"]+)['"]"#,
        ))
        .expect("valid regex")
    });

    let mut deps = Vec::new();
    let mut candidates = extract_named_brace_blocks_from_stripped(contents, "dependencies");
    if candidates.is_empty() {
        candidates.push(contents.to_string());
    }

    for candidate in candidates {
        let candidate = scrub_gradle_dependency_constraint_blocks(&candidate);
        let string_ranges = gradle_string_literal_ranges(&candidate);
        for re in [re_parens, re_no_parens] {
            for caps in re.captures_iter(&candidate) {
                let Some(m0) = caps.get(0) else {
                    continue;
                };
                if is_index_inside_string_ranges(m0.start(), &string_ranges) {
                    continue;
                }
                let Some(project_path) = caps.get(1).map(|m| m.as_str()) else {
                    continue;
                };
                let project_path = project_path.trim();
                if project_path.is_empty() {
                    continue;
                }
                deps.push(normalize_project_path(project_path));
            }
        }
    }
    deps
}

fn resolve_gradle_dependency_version(
    raw_version: &str,
    gradle_properties: &GradleProperties,
) -> Option<String> {
    let raw_version = raw_version.trim();
    if raw_version.is_empty() {
        return None;
    }

    let resolved = if raw_version.contains('$') {
        interpolate_gradle_placeholders(raw_version, gradle_properties)?
    } else {
        raw_version.to_string()
    };
    let resolved = resolved.trim();
    if resolved.is_empty() {
        return None;
    }
    if is_dynamic_gradle_version(resolved) {
        return None;
    }

    Some(resolved.to_string())
}

fn is_dynamic_gradle_version(version: &str) -> bool {
    let version = version.trim();
    if version.contains('$') {
        return true;
    }

    // Gradle dynamic version patterns like:
    // - `+`
    // - `1.+`
    // - `1.2.+`
    //
    // Note: some ecosystems (e.g. SemVer build metadata) can include `+` in a *static* version like
    // `1.0.0+build.1`. Gradle's dynamic selector uses `+` as a wildcard suffix, so we only treat
    // it as dynamic when it appears at the end.
    if version.ends_with('+') {
        return true;
    }

    // Gradle dynamic version selector (see `DynamicVersion` in Gradle).
    matches!(
        version.to_ascii_lowercase().as_str(),
        "latest.release" | "latest.integration"
    )
}

fn interpolate_gradle_placeholders(
    input: &str,
    gradle_properties: &GradleProperties,
) -> Option<String> {
    const MAX_ITERATIONS: usize = 4;

    let mut current = input.to_string();
    for _ in 0..MAX_ITERATIONS {
        let next = interpolate_gradle_placeholders_once(&current, gradle_properties)?;
        if next == current {
            break;
        }
        current = next;
    }
    Some(current)
}

fn interpolate_gradle_placeholders_once(
    input: &str,
    gradle_properties: &GradleProperties,
) -> Option<String> {
    static RE_BRACED: OnceLock<Regex> = OnceLock::new();
    static RE_SIMPLE: OnceLock<Regex> = OnceLock::new();
    let re_braced =
        RE_BRACED.get_or_init(|| Regex::new(r"\$\{([A-Za-z0-9_.-]+)\}").expect("valid regex"));
    let re_simple =
        RE_SIMPLE.get_or_init(|| Regex::new(r"\$([A-Za-z0-9_.-]+)").expect("valid regex"));

    let mut unknown = false;
    let after_braced = re_braced.replace_all(input, |caps: &regex::Captures<'_>| {
        let key = &caps[1];
        if let Some(value) = gradle_properties.get(key) {
            value.clone()
        } else {
            unknown = true;
            caps[0].to_string()
        }
    });
    if unknown {
        return None;
    }

    let mut unknown = false;
    let after_simple = re_simple.replace_all(&after_braced, |caps: &regex::Captures<'_>| {
        let key = &caps[1];
        if let Some(value) = gradle_properties.get(key) {
            value.clone()
        } else {
            unknown = true;
            caps[0].to_string()
        }
    });
    if unknown {
        return None;
    }

    Some(after_simple.into_owned())
}

fn parse_gradle_root_subprojects_allprojects_dependencies(
    workspace_root: &Path,
    version_catalog: Option<&GradleVersionCatalog>,
    gradle_properties: &GradleProperties,
) -> (Vec<Dependency>, Vec<Dependency>) {
    let candidates = ["build.gradle.kts", "build.gradle"]
        .into_iter()
        .map(|name| workspace_root.join(name))
        .filter(|p| p.is_file())
        .collect::<Vec<_>>();

    let mut subprojects = Vec::new();
    let mut allprojects = Vec::new();

    for path in candidates {
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };

        for subproject_block in extract_named_brace_blocks(&contents, "subprojects") {
            for deps_block in extract_named_brace_blocks(&subproject_block, "dependencies") {
                subprojects.extend(parse_gradle_dependencies_from_text(
                    &deps_block,
                    version_catalog,
                    gradle_properties,
                ));
            }
        }

        for allprojects_block in extract_named_brace_blocks(&contents, "allprojects") {
            for deps_block in extract_named_brace_blocks(&allprojects_block, "dependencies") {
                allprojects.extend(parse_gradle_dependencies_from_text(
                    &deps_block,
                    version_catalog,
                    gradle_properties,
                ));
            }
        }
    }

    sort_dedup_dependencies(&mut subprojects);
    sort_dedup_dependencies(&mut allprojects);
    (subprojects, allprojects)
}

fn transitive_gradle_project_dependencies(
    project_path: &str,
    direct_deps: &BTreeMap<String, Vec<String>>,
) -> Vec<String> {
    fn visit(
        current: &str,
        direct_deps: &BTreeMap<String, Vec<String>>,
        seen: &mut BTreeSet<String>,
        out: &mut Vec<String>,
    ) {
        let Some(deps) = direct_deps.get(current) else {
            return;
        };

        let mut deps = deps.clone();
        deps.sort();
        deps.dedup();
        for dep in deps {
            if !seen.insert(dep.clone()) {
                continue;
            }
            out.push(dep.clone());
            visit(&dep, direct_deps, seen, out);
        }
    }

    let mut out = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    seen.insert(project_path.to_string());
    visit(project_path, direct_deps, &mut seen, &mut out);
    out
}

/// Best-effort extraction of local classpath entries from Gradle build scripts.
///
/// This is intended to cover common patterns like:
/// - Groovy DSL: `implementation files('libs/foo.jar')`
/// - Groovy DSL: `implementation fileTree(dir: 'libs', include: ['*.jar'])`
/// - Kotlin DSL: `implementation(files("libs/foo.jar"))`
///
/// This does **not** attempt full Gradle dependency resolution.
fn parse_gradle_local_classpath_entries(module_root: &Path) -> Vec<ClasspathEntry> {
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
        out.extend(parse_gradle_local_classpath_entries_from_text(
            module_root,
            &contents,
        ));
    }
    out
}

fn parse_gradle_local_classpath_entries_from_text(
    module_root: &Path,
    contents: &str,
) -> Vec<ClasspathEntry> {
    static FILE_TREE_DIR_ARG_RE: OnceLock<Regex> = OnceLock::new();
    static FILE_TREE_MAP_DIR_ARG_RE: OnceLock<Regex> = OnceLock::new();
    static CONFIG_RE: OnceLock<Regex> = OnceLock::new();

    // Note: this intentionally keeps the matcher simple; Gradle scripts are not trivially
    // parseable without a real Groovy/Kotlin parser.
    //
    // We first extract a `fileTree(...)` argument list using the Groovy-aware balanced-parens
    // scanner, then apply small regexes *within that argument list* to pull out the `dir` value.
    // This avoids false negatives caused by `)` inside quoted strings.
    let file_tree_dir_arg_re = FILE_TREE_DIR_ARG_RE.get_or_init(|| {
        Regex::new(
            r#"(?s)\bdir\s*(?:[:=])\s*(?:(?:[\w.]+\.)?file\s*\(\s*)?['"](?P<dir>[^'"]+)['"]"#,
        )
        .expect("valid regex")
    });
    let file_tree_map_dir_arg_re = FILE_TREE_MAP_DIR_ARG_RE.get_or_init(|| {
        // Kotlin DSL also supports `fileTree(mapOf("dir" to "libs", ...))` style configuration.
        Regex::new(r#"(?s)['"]dir['"]\s*to\s*(?:file\s*\(\s*)?['"](?P<dir>[^'"]+)['"]"#)
            .expect("valid regex")
    });

    let config_re = CONFIG_RE.get_or_init(|| {
        let configs = GRADLE_DEPENDENCY_CONFIGS;
        Regex::new(&format!(r#"(?i)^{configs}$"#)).expect("valid regex")
    });

    let stripped = strip_gradle_comments(contents);
    let candidates = extract_named_brace_blocks_from_stripped(&stripped, "dependencies");
    if candidates.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::new();

    fn preceding_identifier<'a>(contents: &'a str, start: usize) -> Option<&'a str> {
        let bytes = contents.as_bytes();
        let mut end = start;
        while end > 0 {
            let b = bytes[end - 1];
            if b.is_ascii_whitespace() || b == b'(' || b == b')' || b == b'.' || b == b',' {
                end -= 1;
                continue;
            }
            break;
        }
        if end == 0 {
            return None;
        }

        let mut begin = end;
        while begin > 0 {
            let b = bytes[begin - 1];
            if b.is_ascii_alphanumeric() || b == b'_' {
                begin -= 1;
                continue;
            }
            break;
        }
        if begin == end {
            return None;
        }
        contents.get(begin..end)
    }

    // `files(...)` and no-parens `files "..."` style calls.
    //
    // Use a balanced-parens extractor rather than a regex so a `)` inside a string literal does
    // not truncate the argument list.
    for candidate in candidates {
        let candidate = scrub_gradle_dependency_constraint_blocks(&candidate);
        let contents = candidate.as_str();

        for start in find_keyword_outside_strings(contents, "files") {
            let Some(prev) = preceding_identifier(contents, start) else {
                continue;
            };
            if !config_re.is_match(prev) {
                continue;
            }

            let mut idx = start + "files".len();
            let bytes = contents.as_bytes();
            while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
                idx += 1;
            }
            if idx >= bytes.len() {
                continue;
            }

            let args = if bytes[idx] == b'(' {
                extract_balanced_parens(contents, idx)
                    .map(|(args, _end)| args)
                    .unwrap_or_default()
            } else {
                extract_unparenthesized_args_until_eol_or_continuation(contents, idx)
            };

            for raw in extract_quoted_strings(&args) {
                let raw = raw.trim();
                if raw.is_empty() {
                    continue;
                }

                let raw_path = PathBuf::from(raw);
                let path = if raw_path.is_absolute() {
                    raw_path
                } else {
                    module_root.join(raw_path)
                };

                if path.is_file() {
                    if path
                        .extension()
                        .and_then(|ext| ext.to_str())
                        .is_some_and(|ext| {
                            ext.eq_ignore_ascii_case("jar") || ext.eq_ignore_ascii_case("jmod")
                        })
                    {
                        out.push(ClasspathEntry {
                            kind: ClasspathEntryKind::Jar,
                            path,
                        });
                    }
                } else if path.is_dir() {
                    out.push(ClasspathEntry {
                        kind: ClasspathEntryKind::Directory,
                        path,
                    });
                }
            }
        }

        let mut add_file_tree_dir = |dir: &str| {
            let dir = dir.trim();
            if dir.is_empty() {
                return;
            }

            let raw_dir = PathBuf::from(dir);
            let dir_path = if raw_dir.is_absolute() {
                raw_dir
            } else {
                module_root.join(raw_dir)
            };

            if !dir_path.is_dir() {
                return;
            }

            for entry in WalkDir::new(&dir_path).into_iter().filter_map(Result::ok) {
                if !entry.file_type().is_file() {
                    continue;
                }
                let path = entry.path();
                if path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| {
                        ext.eq_ignore_ascii_case("jar") || ext.eq_ignore_ascii_case("jmod")
                    })
                {
                    out.push(ClasspathEntry {
                        kind: ClasspathEntryKind::Jar,
                        path: path.to_path_buf(),
                    });
                }
            }
        };

        // `fileTree(...)` calls (named args, mapOf, or positional `"libs"` form).
        for start in find_keyword_outside_strings(contents, "fileTree") {
            let Some(prev) = preceding_identifier(contents, start) else {
                continue;
            };
            if !config_re.is_match(prev) {
                continue;
            }

            let mut idx = start + "fileTree".len();
            let bytes = contents.as_bytes();
            while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
                idx += 1;
            }
            if idx >= bytes.len() {
                continue;
            }

            let args = if bytes[idx] == b'(' {
                extract_balanced_parens(contents, idx)
                    .map(|(args, _end)| args)
                    .unwrap_or_default()
            } else {
                extract_unparenthesized_args_until_eol_or_continuation(contents, idx)
            };

            if let Some(dir) = file_tree_map_dir_arg_re
                .captures(&args)
                .and_then(|caps| caps.name("dir").map(|m| m.as_str()))
            {
                add_file_tree_dir(dir);
                continue;
            }

            if let Some(dir) = file_tree_dir_arg_re
                .captures(&args)
                .and_then(|caps| caps.name("dir").map(|m| m.as_str()))
            {
                add_file_tree_dir(dir);
                continue;
            }

            if let Some(dir) = extract_quoted_strings(&args).into_iter().next() {
                add_file_tree_dir(&dir);
            }
        }
    }

    out
}

fn parse_gradle_dependencies_from_text(
    contents: &str,
    version_catalog: Option<&GradleVersionCatalog>,
    gradle_properties: &GradleProperties,
) -> Vec<Dependency> {
    let mut deps = Vec::new();

    // Strip comments before scanning dependency blocks so commented-out dependency lines don't end
    // up polluting the extracted dependency list. This is best-effort but preserves quoted strings,
    // so typical Gradle/Maven coordinate literals are unaffected.
    let stripped = strip_gradle_comments(contents);
    let mut candidates = extract_named_brace_blocks_from_stripped(&stripped, "dependencies");
    if candidates.is_empty() {
        candidates.push(stripped);
    }

    // `implementation "g:a:v"` and similar (string notation).
    //
    // We also intentionally support coordinates *without* an explicit version (`group:artifact`),
    // because many Gradle builds supply versions via plugins/BOMs/version catalogs.
    static RE_GAV: OnceLock<Regex> = OnceLock::new();
    let re_gav = RE_GAV.get_or_init(|| {
        // Keep this list conservative: only configurations that are commonly used for
        // Java compilation or annotation processing. This is best-effort dependency extraction,
        // not a full Gradle parser.
        let configs = GRADLE_DEPENDENCY_CONFIGS;

        Regex::new(&format!(
            r#"(?i)\b(?P<config>{configs})\b\s*\(?\s*{wrappers}['"](?P<group>[^:'"]+):(?P<artifact>[^:'"]+)(?::(?P<version>[^:'"@]+)(?::(?P<classifier>[^:'"@]+))?)?(?:@(?P<type>[^'"]+))?['"]"#,
            wrappers = GRADLE_DEPENDENCY_WRAPPER_PREFIX_RE,
        ))
        .expect("valid regex")
    });

    // `implementation group: 'g', name: 'a', version: 'v'` (Groovy map notation),
    // `implementation(group = "g", name = "a", version = "v")` (Kotlin named args),
    // and similar.
    //
    // We also handle the common case where `version` is omitted:
    //   implementation group: 'g', name: 'a'
    //
    // This is intentionally best-effort (regex-based): it aims to capture the common Groovy
    // "map notation" used in many real-world builds. It is not intended to be a complete Gradle
    // language parser.
    static RE_MAP: OnceLock<Regex> = OnceLock::new();
    let re_map = RE_MAP.get_or_init(|| {
        let configs = GRADLE_DEPENDENCY_CONFIGS;

        // Notes:
        // - `\s` matches newlines in Rust regex, which lets us handle typical multi-line map args.
        // - We accept both `implementation group: ...` and `implementation(group: ...)` forms.
        // - We don't try to parse non-literal versions (variables, method calls, etc).
        Regex::new(&format!(
            r#"(?is)\b(?P<config>{configs})\b\s*\(?\s*{wrappers}group\s*[:=]\s*['"](?P<group>[^'"]+)['"]\s*,\s*(?:name|module)\s*[:=]\s*['"](?P<artifact>[^'"]+)['"](?:\s*,\s*version\s*[:=]\s*['"](?P<version>[^'"]+)['"])?"#,
            wrappers = GRADLE_DEPENDENCY_WRAPPER_PREFIX_RE,
        ))
        .expect("valid regex")
    });

    for candidate in candidates {
        let candidate = scrub_gradle_dependency_constraint_blocks(&candidate);
        let contents = candidate.as_str();
        let string_ranges = gradle_string_literal_ranges(contents);

        for caps in re_gav.captures_iter(contents) {
            let Some(m0) = caps.get(0) else {
                continue;
            };
            if is_index_inside_string_ranges(m0.start(), &string_ranges) {
                continue;
            }
            let Some(config) = caps.name("config").map(|m| m.as_str()) else {
                continue;
            };
            let scope = gradle_scope_from_configuration(config).map(str::to_string);
            let version = caps
                .name("version")
                .and_then(|m| resolve_gradle_dependency_version(m.as_str(), gradle_properties));
            let classifier = caps
                .name("classifier")
                .map(|m| m.as_str().trim())
                .filter(|v| !v.is_empty())
                .map(str::to_string);
            let type_ = caps
                .name("type")
                .map(|m| m.as_str().trim())
                .filter(|v| !v.is_empty())
                .map(str::to_string);
            deps.push(Dependency {
                group_id: caps["group"].to_string(),
                artifact_id: caps["artifact"].to_string(),
                version,
                scope,
                classifier,
                type_,
            });
        }

        for caps in re_map.captures_iter(contents) {
            let Some(m0) = caps.get(0) else {
                continue;
            };
            if is_index_inside_string_ranges(m0.start(), &string_ranges) {
                continue;
            }
            let Some(config) = caps.name("config").map(|m| m.as_str()) else {
                continue;
            };
            let scope = gradle_scope_from_configuration(config).map(str::to_string);
            let version = caps
                .name("version")
                .and_then(|m| resolve_gradle_dependency_version(m.as_str(), gradle_properties));
            deps.push(Dependency {
                group_id: caps["group"].to_string(),
                artifact_id: caps["artifact"].to_string(),
                version,
                scope,
                classifier: None,
                type_: None,
            });
        }

        // Version catalog references (`implementation(libs.foo)` / `implementation libs.foo`).
        if let Some(version_catalog) = version_catalog {
            deps.extend(resolve_version_catalog_dependencies(
                contents,
                version_catalog,
                gradle_properties,
                &string_ranges,
            ));
        }
    }
    sort_dedup_dependencies(&mut deps);
    deps
}

fn scrub_gradle_dependency_constraint_blocks(contents: &str) -> String {
    // `constraints { ... }` blocks inside `dependencies { ... }` don't add artifacts to the
    // classpath; they only constrain versions. Treating them as dependencies produces false
    // positives, so we scrub them before regex extraction.
    let mut ranges = Vec::<std::ops::Range<usize>>::new();
    let bytes = contents.as_bytes();

    for keyword in ["constraints", "dependencyConstraints"] {
        for start in find_keyword_positions_outside_strings(contents, keyword) {
            let mut idx = start + keyword.len();
            while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
                idx += 1;
            }
            if idx >= bytes.len() {
                continue;
            }

            // Handle `keyword(...) { ... }` form by skipping a single balanced `(...)` argument list.
            if bytes[idx] == b'(' {
                if let Some((_args, end)) = extract_balanced_parens(contents, idx) {
                    idx = end;
                    while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
                        idx += 1;
                    }
                }
            }

            if idx >= bytes.len() || bytes[idx] != b'{' {
                continue;
            }

            if let Some((_body, end)) = extract_balanced_braces(contents, idx) {
                ranges.push(start..end);
            }
        }
    }

    if ranges.is_empty() {
        return contents.to_string();
    }

    let mut out = contents.as_bytes().to_vec();
    for range in ranges {
        for b in &mut out[range] {
            if *b != b'\n' {
                *b = b' ';
            }
        }
    }

    String::from_utf8(out).unwrap_or_else(|_| contents.to_string())
}

fn resolve_version_catalog_dependencies(
    contents: &str,
    version_catalog: &GradleVersionCatalog,
    gradle_properties: &GradleProperties,
    string_ranges: &[Range<usize>],
) -> Vec<Dependency> {
    static RE_DOT: OnceLock<Regex> = OnceLock::new();
    static RE_BRACKET: OnceLock<Regex> = OnceLock::new();
    static RE_BUNDLE_BRACKET: OnceLock<Regex> = OnceLock::new();

    let re_dot = RE_DOT.get_or_init(|| {
        let configs = GRADLE_DEPENDENCY_CONFIGS;
        Regex::new(&format!(
            r#"(?i)\b(?P<config>{configs})\b\s*\(?\s*{wrappers}libs\.(?P<ref>[A-Za-z0-9_]+(?:\.[A-Za-z0-9_]+)*)(?:\.get\(\))?(?:\s*\)|\s*,|\s|$)"#,
            wrappers = GRADLE_DEPENDENCY_WRAPPER_PREFIX_RE,
        ))
        .expect("valid regex")
    });
    let re_bracket = RE_BRACKET.get_or_init(|| {
        let configs = GRADLE_DEPENDENCY_CONFIGS;
        Regex::new(&format!(
            r#"(?i)\b(?P<config>{configs})\b\s*\(?\s*{wrappers}libs\s*\[\s*['"](?P<ref>[^'"]+)['"]\s*\](?:\.get\(\))?(?:\s*\)|\s*,|\s|$)"#,
            wrappers = GRADLE_DEPENDENCY_WRAPPER_PREFIX_RE,
        ))
        .expect("valid regex")
    });
    let re_bundle_bracket = RE_BUNDLE_BRACKET.get_or_init(|| {
        let configs = GRADLE_DEPENDENCY_CONFIGS;
        Regex::new(&format!(
            r#"(?i)\b(?P<config>{configs})\b\s*\(?\s*{wrappers}libs\.bundles\s*\[\s*['"](?P<bundle>[^'"]+)['"]\s*\](?:\.get\(\))?(?:\s*\)|\s*,|\s|$)"#,
            wrappers = GRADLE_DEPENDENCY_WRAPPER_PREFIX_RE,
        ))
        .expect("valid regex")
    });

    let mut deps = Vec::new();
    for caps in re_dot.captures_iter(contents) {
        let Some(m0) = caps.get(0) else {
            continue;
        };
        if is_index_inside_string_ranges(m0.start(), string_ranges) {
            continue;
        }
        let Some(reference) = caps.name("ref").map(|m| m.as_str()) else {
            continue;
        };
        let Some(config) = caps.name("config").map(|m| m.as_str()) else {
            continue;
        };
        let scope = gradle_scope_from_configuration(config).map(str::to_string);
        let mut resolved = resolve_version_catalog_reference(version_catalog, reference);
        for dep in &mut resolved {
            dep.scope = scope.clone();
            if let Some(v) = dep.version.as_deref() {
                dep.version = resolve_gradle_dependency_version(v, gradle_properties);
            }
        }
        deps.extend(resolved);
    }

    for caps in re_bracket.captures_iter(contents) {
        let Some(m0) = caps.get(0) else {
            continue;
        };
        if is_index_inside_string_ranges(m0.start(), string_ranges) {
            continue;
        }
        let Some(reference) = caps.name("ref").map(|m| m.as_str()) else {
            continue;
        };
        let scope = caps
            .name("config")
            .and_then(|m| gradle_scope_from_configuration(m.as_str()))
            .map(str::to_string);
        let mut resolved = resolve_version_catalog_reference(version_catalog, reference);
        if let Some(scope) = scope {
            for dep in &mut resolved {
                dep.scope = Some(scope.clone());
            }
        }
        for dep in &mut resolved {
            if let Some(v) = dep.version.as_deref() {
                dep.version = resolve_gradle_dependency_version(v, gradle_properties);
            }
        }
        deps.extend(resolved);
    }

    for caps in re_bundle_bracket.captures_iter(contents) {
        let Some(m0) = caps.get(0) else {
            continue;
        };
        if is_index_inside_string_ranges(m0.start(), string_ranges) {
            continue;
        }
        let Some(bundle) = caps.name("bundle").map(|m| m.as_str()) else {
            continue;
        };
        let scope = caps
            .name("config")
            .and_then(|m| gradle_scope_from_configuration(m.as_str()))
            .map(str::to_string);
        let reference = format!("bundles.{bundle}");
        let mut resolved = resolve_version_catalog_reference(version_catalog, &reference);
        if let Some(scope) = scope {
            for dep in &mut resolved {
                dep.scope = Some(scope.clone());
            }
        }
        for dep in &mut resolved {
            if let Some(v) = dep.version.as_deref() {
                dep.version = resolve_gradle_dependency_version(v, gradle_properties);
            }
        }
        deps.extend(resolved);
    }

    deps
}

fn resolve_version_catalog_reference(
    version_catalog: &GradleVersionCatalog,
    reference: &str,
) -> Vec<Dependency> {
    let reference = reference.trim();
    if reference.is_empty() {
        return Vec::new();
    }

    if let Some(bundle_accessor) = reference.strip_prefix("bundles.") {
        let Some(bundle) = resolve_version_catalog_key(&version_catalog.bundles, bundle_accessor)
        else {
            return Vec::new();
        };

        let mut deps = Vec::new();
        for lib_alias in bundle {
            let Some(lib) = version_catalog
                .libraries
                .get(lib_alias)
                .or_else(|| resolve_version_catalog_key(&version_catalog.libraries, lib_alias))
            else {
                continue;
            };
            deps.extend(version_catalog_library_to_dependency(lib));
        }
        return deps;
    }

    let Some(lib) = resolve_version_catalog_key(&version_catalog.libraries, reference) else {
        return Vec::new();
    };

    version_catalog_library_to_dependency(lib)
        .into_iter()
        .collect()
}

fn version_catalog_library_to_dependency(lib: &GradleVersionCatalogLibrary) -> Option<Dependency> {
    if lib.group_id.trim().is_empty() || lib.artifact_id.trim().is_empty() {
        return None;
    }
    Some(Dependency {
        group_id: lib.group_id.clone(),
        artifact_id: lib.artifact_id.clone(),
        version: lib.version.clone(),
        scope: None,
        classifier: None,
        type_: None,
    })
}

fn resolve_version_catalog_key<'a, T>(
    map: &'a HashMap<String, T>,
    accessor: &str,
) -> Option<&'a T> {
    for candidate in version_catalog_key_candidates(accessor) {
        if let Some(v) = map.get(&candidate) {
            return Some(v);
        }
    }
    None
}

fn version_catalog_key_candidates(accessor: &str) -> Vec<String> {
    let accessor = accessor.trim();
    if accessor.is_empty() {
        return Vec::new();
    }

    let segments = accessor.split('.').collect::<Vec<_>>();
    let mut out = Vec::new();

    let push_unique = |out: &mut Vec<String>, s: String| {
        if !out.iter().any(|existing| existing == &s) {
            out.push(s);
        }
    };

    push_unique(&mut out, segments.join("."));
    push_unique(&mut out, segments.join("-"));
    push_unique(&mut out, segments.join("_"));

    out
}

fn default_gradle_user_home() -> Option<PathBuf> {
    fn home_dir() -> Option<PathBuf> {
        std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    }

    fn expand_tilde_home(value: &str) -> Option<PathBuf> {
        let rest = value.strip_prefix('~')?;
        let home = home_dir()?;

        if rest.is_empty() {
            return Some(home);
        }

        // Only expand `~/...` (or `~\\...` on Windows). Don't guess for `~user/...`.
        let rest = rest.strip_prefix('/').or_else(|| rest.strip_prefix('\\'))?;
        if rest.contains("${") {
            return None;
        }

        Some(home.join(rest))
    }

    fn expand_user_home_placeholder(value: &str) -> Option<PathBuf> {
        const USER_HOME: &str = "${user.home}";
        let rest = value.strip_prefix(USER_HOME)?;

        let home = home_dir()?;
        if rest.is_empty() {
            return Some(home);
        }

        // Accept both separators so configs remain portable.
        let rest = rest
            .strip_prefix('/')
            .or_else(|| rest.strip_prefix('\\'))
            .unwrap_or(rest);
        if rest.contains("${") {
            // If there are any remaining placeholders, bail out rather than guessing.
            return None;
        }

        Some(home.join(rest))
    }

    fn expand_env_placeholder(value: &str) -> Option<PathBuf> {
        const PREFIX: &str = "${env.";
        let rest = value.strip_prefix(PREFIX)?;
        let (raw_key, rest) = rest.split_once('}')?;
        let key = raw_key.trim();
        if key.is_empty() {
            return None;
        }

        let base = PathBuf::from(std::env::var_os(key)?);
        if rest.is_empty() {
            return Some(base);
        }

        // Accept both separators so configs remain portable.
        let rest = rest
            .strip_prefix('/')
            .or_else(|| rest.strip_prefix('\\'))
            .unwrap_or(rest);
        if rest.contains("${") {
            return None;
        }

        Some(base.join(rest))
    }

    if let Some(home) = std::env::var_os("GRADLE_USER_HOME").filter(|v| !v.is_empty()) {
        let value = home.to_string_lossy();
        let value = value
            .trim()
            .trim_matches(|c| matches!(c, '"' | '\''))
            .trim();
        if !value.is_empty() {
            if let Some(expanded) = expand_tilde_home(value)
                .or_else(|| expand_env_placeholder(value))
                .or_else(|| expand_user_home_placeholder(value))
            {
                return Some(expanded);
            }
            if !value.contains("${") {
                return Some(PathBuf::from(value));
            }
        }
    }

    Some(home_dir()?.join(".gradle"))
}

/// Best-effort jar discovery for Gradle dependencies.
///
/// This does **not** run Gradle and does **not** resolve transitive dependencies
/// or perform variant/attribute selection. It only attempts to locate jar files
/// for explicitly-versioned Maven coordinates that already exist in the local
/// Gradle cache.
fn gradle_dependency_jar_paths(gradle_user_home: &Path, dep: &Dependency) -> Vec<PathBuf> {
    let Some(version) = dep.version.as_deref() else {
        return Vec::new();
    };
    if dep.group_id.is_empty() || dep.artifact_id.is_empty() || version.is_empty() {
        return Vec::new();
    }

    let base = gradle_user_home
        .join("caches/modules-2/files-2.1")
        .join(&dep.group_id)
        .join(&dep.artifact_id)
        .join(version);
    if !base.is_dir() {
        return Vec::new();
    }

    let base_prefix = format!("{}-{}", dep.artifact_id, version);
    let preferred_prefix = dep
        .classifier
        .as_deref()
        .filter(|c| !c.is_empty())
        .map(|c| format!("{base_prefix}-{c}"))
        .unwrap_or_else(|| base_prefix.clone());

    let mut preferred = Vec::new();
    let mut fallback = Vec::new();
    let mut others = Vec::new();

    for entry in WalkDir::new(&base).into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.into_path();

        if !is_jar_path(&path) || is_auxiliary_gradle_jar(&path) {
            continue;
        }

        let file_name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default();

        if file_name.starts_with(&preferred_prefix) {
            preferred.push(path);
        } else if dep.classifier.is_some() && file_name.starts_with(&base_prefix) {
            fallback.push(path);
        } else {
            others.push(path);
        }
    }

    let mut out = if !preferred.is_empty() {
        preferred
    } else if !fallback.is_empty() {
        fallback
    } else {
        others
    };
    out.sort();
    out.dedup();
    out
}

fn is_jar_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("jar"))
}

fn is_auxiliary_gradle_jar(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    name.ends_with("-sources.jar") || name.ends_with("-javadoc.jar")
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

fn append_source_set_java_roots(out: &mut Vec<SourceRoot>, module_root: &Path) {
    let src_dir = module_root.join("src");
    let Ok(entries) = std::fs::read_dir(src_dir) else {
        return;
    };

    let mut source_sets = entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect::<Vec<_>>();
    source_sets.sort();

    for source_set in source_sets {
        let kind = gradle_source_set_kind(&source_set);
        let rel = format!("src/{source_set}/java");
        push_source_root(out, module_root, kind, &rel);
    }
}

fn gradle_source_set_kind(source_set: &str) -> SourceRootKind {
    if source_set.eq_ignore_ascii_case("main") {
        return SourceRootKind::Main;
    }
    if source_set.eq_ignore_ascii_case("test") {
        return SourceRootKind::Test;
    }

    if source_set.to_ascii_lowercase().contains("test") {
        SourceRootKind::Test
    } else {
        SourceRootKind::Main
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
            .then(a.classifier.cmp(&b.classifier))
            .then(a.type_.cmp(&b.type_))
    });

    fn scope_precedence(scope: Option<&str>) -> u8 {
        match scope {
            Some("annotationProcessor") => 5,
            Some("compile") => 4,
            Some("runtime") => 3,
            Some("provided") => 2,
            Some("test") => 1,
            Some(_) => 0,
            None => 0,
        }
    }

    fn merge_scope(existing: &mut Option<String>, incoming: Option<String>) {
        let Some(incoming) = incoming else {
            return;
        };

        // `compile` is the closest single-scope approximation for dependencies that are needed at
        // both compile-time (`compileOnly` -> `provided`) and runtime (`runtimeOnly` -> `runtime`).
        //
        // Example: if a dependency is declared in both `compileOnly` and `runtimeOnly`, it must be
        // present on both the compile classpath and runtime classpath, so collapsing it to
        // `compile` is the most permissive stable scope.
        if matches!(
            (existing.as_deref(), incoming.as_str()),
            (Some("runtime"), "provided") | (Some("provided"), "runtime")
        ) {
            *existing = Some("compile".to_string());
            return;
        }

        let existing_rank = scope_precedence(existing.as_deref());
        let incoming_rank = scope_precedence(Some(incoming.as_str()));
        if incoming_rank > existing_rank {
            *existing = Some(incoming);
            return;
        }
        if incoming_rank < existing_rank {
            return;
        }

        // Deterministic tie-breaker for equal-precedence scopes: prefer `Some`, and then prefer the
        // lexicographically smallest scope string.
        match existing.as_deref() {
            None => *existing = Some(incoming),
            Some(cur) => {
                if incoming.as_str() < cur {
                    *existing = Some(incoming);
                }
            }
        }
    }

    let mut out: Vec<Dependency> = Vec::with_capacity(deps.len());
    for dep in std::mem::take(deps) {
        if let Some(last) = out.last_mut() {
            if last.group_id == dep.group_id
                && last.artifact_id == dep.artifact_id
                && last.version == dep.version
                && last.classifier == dep.classifier
                && last.type_ == dep.type_
            {
                merge_scope(&mut last.scope, dep.scope);
                continue;
            }
        }
        out.push(dep);
    }
    *deps = out;
}

fn same_dependency_identity(a: &Dependency, b: &Dependency) -> bool {
    a.group_id == b.group_id
        && a.artifact_id == b.artifact_id
        && a.version == b.version
        && a.classifier == b.classifier
        && a.type_ == b.type_
}

fn retain_dependencies_not_in(deps: &mut Vec<Dependency>, remove: &[Dependency]) {
    if deps.is_empty() || remove.is_empty() {
        return;
    }
    deps.retain(|dep| {
        !remove
            .iter()
            .any(|other| same_dependency_identity(dep, other))
    });
}

fn canonicalize_or_fallback(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn sort_dedup_modules(modules: &mut Vec<Module>, workspace_root: &Path) {
    let buildsrc_root = workspace_root.join("buildSrc");
    modules.sort_by(|a, b| {
        let a_is_root = a.root == workspace_root;
        let b_is_root = b.root == workspace_root;
        let a_is_buildsrc = a.root == buildsrc_root;
        let b_is_buildsrc = b.root == buildsrc_root;
        b_is_root
            .cmp(&a_is_root)
            .then_with(|| b_is_buildsrc.cmp(&a_is_buildsrc))
            .then_with(|| a.root.cmp(&b.root))
            .then_with(|| a.name.cmp(&b.name))
    });
    modules.dedup_by(|a, b| a.root == b.root);
}

fn sort_dedup_workspace_modules(modules: &mut Vec<WorkspaceModuleConfig>) {
    modules.sort_by(|a, b| {
        let a_is_root = matches!(
            &a.build_id,
            WorkspaceModuleBuildId::Gradle { project_path } if project_path == ":"
        );
        let b_is_root = matches!(
            &b.build_id,
            WorkspaceModuleBuildId::Gradle { project_path } if project_path == ":"
        );
        let a_is_buildsrc = matches!(
            &a.build_id,
            WorkspaceModuleBuildId::Gradle { project_path } if project_path == GRADLE_BUILDSRC_PROJECT_PATH
        );
        let b_is_buildsrc = matches!(
            &b.build_id,
            WorkspaceModuleBuildId::Gradle { project_path } if project_path == GRADLE_BUILDSRC_PROJECT_PATH
        );

        b_is_root
            .cmp(&a_is_root)
            .then_with(|| b_is_buildsrc.cmp(&a_is_buildsrc))
            .then_with(|| a.root.cmp(&b.root))
            .then_with(|| a.id.cmp(&b.id))
    });
    modules.dedup_by(|a, b| a.root == b.root);
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;

    use super::{
        append_included_build_module_refs, default_gradle_user_home, extract_named_brace_blocks,
        gradle_dependency_jar_paths, parse_gradle_dependencies_from_text,
        parse_gradle_local_classpath_entries_from_text,
        parse_gradle_project_dependencies_from_text, parse_gradle_settings_included_builds,
        parse_gradle_settings_projects, parse_gradle_version_catalog_from_toml,
        sort_dedup_dependencies, strip_gradle_comments, ClasspathEntryKind, Dependency,
        GradleModuleRef, GradleProperties,
    };
    use crate::test_support::{env_lock, EnvVarGuard};
    use tempfile::tempdir;

    #[test]
    fn parse_gradle_settings_projects_ignores_include_keywords_inside_strings() {
        // `include` / `includeFlat` are parsed via keyword matching. Ensure we don't treat
        // occurrences inside string literals as settings directives.
        let settings = r#"
rootProject.name = "includeFlat-root"

// Kotlin-style var assignment + concatenation.
val ignoredFlat = "includeFlat" + "app"
val ignoredInclude = "include" + ":lib"

// Triple-quoted strings (Groovy/Kotlin raw) should also be ignored.
val triple = """include(":app")"""
def tripleGroovy = '''includeFlat("lib")'''
"#;

        let modules = parse_gradle_settings_projects(settings);
        assert_eq!(modules.len(), 1);
        assert_eq!(modules[0].project_path, ":");
        assert_eq!(modules[0].dir_rel, ".");
    }

    #[test]
    fn parse_gradle_settings_projects_ignores_include_keywords_inside_slashy_strings() {
        let settings = r#"
def ignored = /include ':ignored'/
def ignored_flat = $/includeFlat ':ignoredFlat'/$

include(":app")
"#;

        let modules = parse_gradle_settings_projects(settings);
        assert_eq!(modules.len(), 1);
        assert_eq!(modules[0].project_path, ":app");
        assert_eq!(modules[0].dir_rel, "app");
    }

    #[test]
    fn parse_gradle_settings_projects_parses_triple_quoted_include_arguments() {
        let settings = r#"
 include("""app""")
 includeFlat('''lib''')
 "#;

        let modules = parse_gradle_settings_projects(settings);
        assert_eq!(modules.len(), 2);
        assert_eq!(modules[0].project_path, ":app");
        assert_eq!(modules[0].dir_rel, "app");
        assert_eq!(modules[1].project_path, ":lib");
        assert_eq!(modules[1].dir_rel, "../lib");
    }

    #[test]
    fn parse_gradle_settings_projects_ignores_projectdir_overrides_inside_strings() {
        let settings = r#"
include(":app")

val ignored = "project(':app').projectDir = file('modules/app')"
val triple = """project(':app').projectDir = file('modules/app')"""
def tripleGroovy = '''project(':app').projectDir = file('modules/app')'''
"#;

        let modules = parse_gradle_settings_projects(settings);
        assert_eq!(modules.len(), 1);
        assert_eq!(modules[0].project_path, ":app");
        assert_eq!(modules[0].dir_rel, "app");
    }

    #[test]
    fn parse_gradle_settings_projects_parses_projectdir_override_with_rootdir_file_constructor() {
        let settings = r#"
include ':app'
project(':app').projectDir = new File(rootDir, 'modules/app')
"#;

        let modules = parse_gradle_settings_projects(settings);
        assert_eq!(modules.len(), 1);
        assert_eq!(modules[0].project_path, ":app");
        assert_eq!(modules[0].dir_rel, "modules/app");
    }

    #[test]
    fn parse_gradle_settings_included_builds_ignores_keywords_inside_strings() {
        let settings = r#"
rootProject.name = "includeBuild-root"

val ignored = "includeBuild('ignored')"

val triple = """includeBuild("ignored2")"""
def tripleGroovy = '''includeBuild("ignored3")'''

// This should be discovered.
includeBuild("build-logic")
"#;
        let builds = parse_gradle_settings_included_builds(settings);
        assert_eq!(builds, vec!["build-logic".to_string()]);
    }

    #[test]
    fn parse_gradle_settings_included_builds_ignores_keywords_inside_slashy_strings() {
        let settings = r#"
def ignored = /includeBuild("ignored")/
def ignored2 = $/includeBuild("ignored2")/$

includeBuild("build-logic")
"#;
        let builds = parse_gradle_settings_included_builds(settings);
        assert_eq!(builds, vec!["build-logic".to_string()]);
    }

    #[test]
    fn parse_gradle_settings_included_builds_ignores_absolute_paths() {
        let settings = r#"
includeBuild("/absolute/path")
includeBuild("C:\\absolute\\path")
includeBuild("build-logic")
"#;
        let builds = parse_gradle_settings_included_builds(settings);
        assert_eq!(builds, vec!["build-logic".to_string()]);
    }

    #[test]
    fn parse_gradle_settings_included_builds_supports_multiline_parens() {
        let settings = r#"
 includeBuild(
     "build-logic"
 )
  "#;
        let builds = parse_gradle_settings_included_builds(settings);
        assert_eq!(builds, vec!["build-logic".to_string()]);
    }

    #[test]
    fn parse_gradle_settings_included_builds_parses_triple_quoted_arguments() {
        let settings = r#"
includeBuild("""build-logic""")
includeBuild '''build-logic2'''
"#;
        let builds = parse_gradle_settings_included_builds(settings);
        assert_eq!(
            builds,
            vec!["build-logic".to_string(), "build-logic2".to_string()]
        );
    }

    #[test]
    fn parse_gradle_settings_included_builds_parses_file_and_named_argument_forms() {
        let settings = r#"
includeBuild(file("build-logic"))
includeBuild(path = "../included")
includeBuild(rootDir = file("../nested"))
"#;
        let builds = parse_gradle_settings_included_builds(settings);
        assert_eq!(
            builds,
            vec![
                "../included".to_string(),
                "../nested".to_string(),
                "build-logic".to_string(),
            ],
        );
    }

    #[test]
    fn append_included_build_module_refs_ignores_missing_or_non_gradle_roots() {
        let dir = tempdir().expect("tempdir");
        let workspace_root = dir.path();
        fs::create_dir_all(workspace_root.join("build-logic")).expect("create build-logic dir");
        fs::write(
            workspace_root.join("build-logic/settings.gradle"),
            "rootProject.name = 'build-logic'",
        )
        .expect("write build-logic settings");

        // Directory exists, but isn't a Gradle build (no `settings.gradle(.kts)` / `build.gradle(.kts)`).
        fs::create_dir_all(workspace_root.join("not-a-build")).expect("create not-a-build dir");

        let mut module_refs = vec![GradleModuleRef::root()];
        let added = append_included_build_module_refs(
            &mut module_refs,
            workspace_root,
            vec![
                "build-logic".to_string(),
                "not-a-build".to_string(),
                "missing".to_string(),
            ],
        );

        assert_eq!(added.len(), 1);
        assert_eq!(added[0].project_path, ":__includedBuild_build-logic");
        assert_eq!(added[0].dir_rel, "build-logic");

        assert!(module_refs.iter().any(|m| m.dir_rel == "build-logic"));
        assert!(!module_refs.iter().any(|m| m.dir_rel == "not-a-build"));
        assert!(!module_refs.iter().any(|m| m.dir_rel == "missing"));
    }

    #[test]
    fn append_included_build_module_refs_dedups_equivalent_roots_and_uses_canonical_basename() {
        let dir = tempdir().expect("tempdir");
        let workspace_root = dir.path();
        fs::create_dir_all(workspace_root.join("build-logic")).expect("create build-logic dir");
        fs::write(
            workspace_root.join("build-logic/settings.gradle"),
            "rootProject.name = 'build-logic'",
        )
        .expect("write build-logic settings");

        let mut module_refs = vec![GradleModuleRef::root()];
        let added = append_included_build_module_refs(
            &mut module_refs,
            workspace_root,
            vec!["build-logic".to_string(), "build-logic/.".to_string()],
        );

        assert_eq!(added.len(), 1);
        assert_eq!(added[0].project_path, ":__includedBuild_build-logic");
        assert_eq!(added[0].dir_rel, "build-logic");
    }

    #[test]
    fn parses_gradle_dependencies_from_text_string_and_map_notation() {
        let script = r#"
plugins {
    id 'java'
}

dependencies {
    // String GAV notation.
    implementation 'org.slf4j:slf4j-api:1.7.36'

    // Groovy map notation (no parens).
    implementation group: 'org.example', name: 'foo', version: '1.2.3'
    implementation platform(group: 'org.example', name: 'wrapped', version: '9.9.9')

    // Map notation with double quotes.
    testImplementation group: "org.example", name: "bar", version: "4.5.6"
    testImplementation(platform(group: "org.example", name: "wrapped2", version: "9.9.8"))

    // Kotlin named args (even in a Groovy file, this is just text for regex extraction).
    implementation(group = "org.example", name = "baz", version = "7.8.9")

    // Map notation with trailing closure and parens (config covered by Task 72).
    annotationProcessor(group: 'com.google.auto.service', name: 'auto-service', version: '1.1.1') {
        // closure content shouldn't matter for extraction
        transitive = false
    }
}
"#;

        let gradle_properties = GradleProperties::new();
        let mut deps = parse_gradle_dependencies_from_text(script, None, &gradle_properties);
        sort_dedup_dependencies(&mut deps);

        let got: BTreeSet<_> = deps
            .into_iter()
            .map(|d| (d.group_id, d.artifact_id, d.version))
            .collect();

        assert!(got.contains(&(
            "org.slf4j".to_string(),
            "slf4j-api".to_string(),
            Some("1.7.36".to_string())
        )));
        assert!(got.contains(&(
            "org.example".to_string(),
            "foo".to_string(),
            Some("1.2.3".to_string())
        )));
        assert!(got.contains(&(
            "org.example".to_string(),
            "wrapped".to_string(),
            Some("9.9.9".to_string())
        )));
        assert!(got.contains(&(
            "org.example".to_string(),
            "bar".to_string(),
            Some("4.5.6".to_string())
        )));
        assert!(got.contains(&(
            "org.example".to_string(),
            "wrapped2".to_string(),
            Some("9.9.8".to_string())
        )));
        assert!(got.contains(&(
            "org.example".to_string(),
            "baz".to_string(),
            Some("7.8.9".to_string())
        )));
        assert!(got.contains(&(
            "com.google.auto.service".to_string(),
            "auto-service".to_string(),
            Some("1.1.1".to_string())
        )));
    }

    #[test]
    fn parses_gradle_dependencies_from_text_classifier_and_type_notation() {
        let script = r#"
dependencies {
    implementation("org.example:foo:1.2.3:linux@jar")
    runtimeOnly 'org.example:bar:4.5.6@jar'
    compileOnly 'org.example:baz:7.8.9:all'
}
"#;

        let gradle_properties = GradleProperties::new();
        let mut deps = parse_gradle_dependencies_from_text(script, None, &gradle_properties);
        sort_dedup_dependencies(&mut deps);

        let foo = deps
            .iter()
            .find(|d| d.group_id == "org.example" && d.artifact_id == "foo")
            .expect("foo dep");
        assert_eq!(foo.version.as_deref(), Some("1.2.3"));
        assert_eq!(foo.classifier.as_deref(), Some("linux"));
        assert_eq!(foo.type_.as_deref(), Some("jar"));
        assert_eq!(foo.scope.as_deref(), Some("compile"));

        let bar = deps
            .iter()
            .find(|d| d.group_id == "org.example" && d.artifact_id == "bar")
            .expect("bar dep");
        assert_eq!(bar.version.as_deref(), Some("4.5.6"));
        assert_eq!(bar.classifier.as_deref(), None);
        assert_eq!(bar.type_.as_deref(), Some("jar"));
        assert_eq!(bar.scope.as_deref(), Some("runtime"));

        let baz = deps
            .iter()
            .find(|d| d.group_id == "org.example" && d.artifact_id == "baz")
            .expect("baz dep");
        assert_eq!(baz.version.as_deref(), Some("7.8.9"));
        assert_eq!(baz.classifier.as_deref(), Some("all"));
        assert_eq!(baz.type_.as_deref(), None);
        assert_eq!(baz.scope.as_deref(), Some("provided"));
    }

    #[test]
    fn parses_gradle_dependencies_from_text_gav_wrappers_and_nested_calls() {
        let script = r#"
dependencies {
  implementation platform("org.example:foo:1.2.3")
  testImplementation(platform("org.example:bar:4.5.6@jar"))
  compileOnly enforcedPlatform("org.example:baz:7.8.9:all")
}
"#;

        let gradle_properties = GradleProperties::new();
        let mut deps = parse_gradle_dependencies_from_text(script, None, &gradle_properties);
        sort_dedup_dependencies(&mut deps);

        let foo = deps
            .iter()
            .find(|d| d.group_id == "org.example" && d.artifact_id == "foo")
            .expect("foo dep");
        assert_eq!(foo.version.as_deref(), Some("1.2.3"));
        assert_eq!(foo.classifier.as_deref(), None);
        assert_eq!(foo.type_.as_deref(), None);
        assert_eq!(foo.scope.as_deref(), Some("compile"));

        let bar = deps
            .iter()
            .find(|d| d.group_id == "org.example" && d.artifact_id == "bar")
            .expect("bar dep");
        assert_eq!(bar.version.as_deref(), Some("4.5.6"));
        assert_eq!(bar.classifier.as_deref(), None);
        assert_eq!(bar.type_.as_deref(), Some("jar"));
        assert_eq!(bar.scope.as_deref(), Some("test"));

        let baz = deps
            .iter()
            .find(|d| d.group_id == "org.example" && d.artifact_id == "baz")
            .expect("baz dep");
        assert_eq!(baz.version.as_deref(), Some("7.8.9"));
        assert_eq!(baz.classifier.as_deref(), Some("all"));
        assert_eq!(baz.type_.as_deref(), None);
        assert_eq!(baz.scope.as_deref(), Some("provided"));
    }

    #[test]
    fn parses_gradle_dependencies_from_text_gav_no_parens_wrappers() {
        let script = r#"
dependencies {
  implementation platform "org.example:foo:1.2.3"
  testImplementation enforcedPlatform 'org.example:bar:4.5.6@jar'
}
"#;

        let gradle_properties = GradleProperties::new();
        let mut deps = parse_gradle_dependencies_from_text(script, None, &gradle_properties);
        sort_dedup_dependencies(&mut deps);

        let foo = deps
            .iter()
            .find(|d| d.group_id == "org.example" && d.artifact_id == "foo")
            .expect("foo dep");
        assert_eq!(foo.version.as_deref(), Some("1.2.3"));
        assert_eq!(foo.type_.as_deref(), None);
        assert_eq!(foo.scope.as_deref(), Some("compile"));

        let bar = deps
            .iter()
            .find(|d| d.group_id == "org.example" && d.artifact_id == "bar")
            .expect("bar dep");
        assert_eq!(bar.version.as_deref(), Some("4.5.6"));
        assert_eq!(bar.type_.as_deref(), Some("jar"));
        assert_eq!(bar.scope.as_deref(), Some("test"));
    }

    #[test]
    fn gradle_dependency_jar_paths_prefers_classifier_matches_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let gradle_home = dir.path();

        let base = gradle_home
            .join("caches/modules-2/files-2.1")
            .join("g")
            .join("a")
            .join("1.0")
            .join("deadbeef");
        std::fs::create_dir_all(&base).unwrap();

        let plain = base.join("a-1.0.jar");
        let classified = base.join("a-1.0-linux.jar");
        std::fs::write(&plain, "").unwrap();
        std::fs::write(&classified, "").unwrap();

        let dep = Dependency {
            group_id: "g".to_string(),
            artifact_id: "a".to_string(),
            version: Some("1.0".to_string()),
            scope: None,
            classifier: Some("linux".to_string()),
            type_: None,
        };

        let jars = gradle_dependency_jar_paths(gradle_home, &dep);
        assert_eq!(jars, vec![classified]);
    }

    #[test]
    fn parses_gradle_dependencies_from_text_ignores_commented_out_dependencies() {
        let script = r#"
dependencies {
    // implementation "com.example:ignored:1"
    /* testImplementation("com.example:ignored2:2") */
    implementation("com.example:kept:3")
}
"#;

        let gradle_properties = GradleProperties::new();
        let deps = parse_gradle_dependencies_from_text(script, None, &gradle_properties);
        let got: BTreeSet<_> = deps
            .into_iter()
            .map(|d| (d.group_id, d.artifact_id, d.version))
            .collect();

        assert!(
            !got.contains(&(
                "com.example".to_string(),
                "ignored".to_string(),
                Some("1".to_string())
            )),
            "commented-out dependency should not be extracted"
        );
        assert!(
            !got.contains(&(
                "com.example".to_string(),
                "ignored2".to_string(),
                Some("2".to_string())
            )),
            "block-commented dependency should not be extracted"
        );
        assert!(
            got.contains(&(
                "com.example".to_string(),
                "kept".to_string(),
                Some("3".to_string())
            )),
            "expected non-commented dependency to be extracted"
        );
    }

    #[test]
    fn parses_gradle_dependencies_from_text_ignores_constraints_blocks() {
        let script = r#"
dependencies {
  constraints {
    implementation("com.example:ignored:1")
    implementation platform("com.example:ignored2:2")
    implementation group: "com.example", name: "ignored3", version: "3"
  }
  implementation("com.example:kept:4")
}
"#;

        let gradle_properties = GradleProperties::new();
        let deps = parse_gradle_dependencies_from_text(script, None, &gradle_properties);
        let got: BTreeSet<_> = deps
            .into_iter()
            .map(|d| (d.group_id, d.artifact_id, d.version))
            .collect();

        assert!(
            !got.contains(&(
                "com.example".to_string(),
                "ignored".to_string(),
                Some("1".to_string())
            )),
            "constraints block should not contribute dependencies"
        );
        assert!(
            !got.contains(&(
                "com.example".to_string(),
                "ignored2".to_string(),
                Some("2".to_string())
            )),
            "constraints wrapper entries should not contribute dependencies"
        );
        assert!(
            !got.contains(&(
                "com.example".to_string(),
                "ignored3".to_string(),
                Some("3".to_string())
            )),
            "constraints map-notation should not contribute dependencies"
        );
        assert!(got.contains(&(
            "com.example".to_string(),
            "kept".to_string(),
            Some("4".to_string())
        )));
    }

    #[test]
    fn gradle_dependency_versions_drop_dynamic_plus_selectors_but_keep_literal_plus_versions() {
        let script = r#"
dependencies {
  implementation("com.example:dynamic:1.+")
  implementation("com.example:literal:1.0.0+build.1")
}
"#;

        let gradle_properties = GradleProperties::new();
        let deps = parse_gradle_dependencies_from_text(script, None, &gradle_properties);

        let got: BTreeSet<_> = deps
            .into_iter()
            .map(|d| (d.group_id, d.artifact_id, d.version))
            .collect();

        assert!(got.contains(&("com.example".to_string(), "dynamic".to_string(), None)));
        assert!(got.contains(&(
            "com.example".to_string(),
            "literal".to_string(),
            Some("1.0.0+build.1".to_string())
        )));
    }

    #[test]
    fn parses_gradle_dependencies_from_text_supported_configurations_and_dedups() {
        let build_script = r#"
plugins {
    kotlin("jvm") version "1.9.0"
}

dependencies {
    implementation("g1:a1:1")
    api("g2:a2:2")
    // Legacy configurations (pre-Gradle 3.4).
    compile("g14:a14:14")
    runtime("g15:a15:15")
    compileOnly("g3:a3:3")
    // Java EE / War plugin style configurations.
    providedCompile("g18:a18:18")
    providedRuntime("g19:a19:19")
    // `java-library` style config for API deps that should still be compile-only.
    compileOnlyApi("g20:a20:20")
    // Some builds still use a plain `provided` configuration.
    provided("g23:a23:23")
    runtimeOnly("g4:a4:4")
    testImplementation("g5:a5:5")
    testCompile("g16:a16:16")
    testRuntime("g17:a17:17")
    testRuntimeOnly("g6:a6:6")
    testCompileOnly("g7:a7:7")
    annotationProcessor("g8:a8:8")
    testAnnotationProcessor("g9:a9:9")
    kapt("g10:a10:10")
    kaptTest("g11:a11:11")
    ksp("g21:a21:21")
    kspTest("g22:a22:22")
    // Legacy/third-party annotation processing plugin configurations.
    apt("g24:a24:24")
    testApt("g25:a25:25")

    // Groovy-style call form (no parens)
    implementation 'g12:a12:12'
    kapt 'g13:a13:13'

    // Duplicate coordinates should not produce duplicates in output.
    implementation("dup:dep:1.0")
    testImplementation("dup:dep:1.0")
}
"#;

        let gradle_properties = GradleProperties::new();
        let deps = parse_gradle_dependencies_from_text(build_script, None, &gradle_properties);

        let mut tuples: Vec<(String, String, Option<String>)> = Vec::new();
        let mut scopes: BTreeMap<(String, String, Option<String>), Option<String>> =
            BTreeMap::new();
        for dep in deps {
            let key = (
                dep.group_id.clone(),
                dep.artifact_id.clone(),
                dep.version.clone(),
            );
            tuples.push(key.clone());
            scopes.insert(key, dep.scope);
        }
        let got: BTreeSet<_> = tuples.iter().cloned().collect();

        let expected: BTreeSet<(String, String, Option<String>)> = [
            ("g1", "a1", "1"),
            ("g2", "a2", "2"),
            ("g3", "a3", "3"),
            ("g4", "a4", "4"),
            ("g5", "a5", "5"),
            ("g6", "a6", "6"),
            ("g7", "a7", "7"),
            ("g8", "a8", "8"),
            ("g9", "a9", "9"),
            ("g10", "a10", "10"),
            ("g11", "a11", "11"),
            ("g12", "a12", "12"),
            ("g13", "a13", "13"),
            ("g14", "a14", "14"),
            ("g15", "a15", "15"),
            ("g16", "a16", "16"),
            ("g17", "a17", "17"),
            ("g18", "a18", "18"),
            ("g19", "a19", "19"),
            ("g20", "a20", "20"),
            ("g21", "a21", "21"),
            ("g22", "a22", "22"),
            ("g23", "a23", "23"),
            ("g24", "a24", "24"),
            ("g25", "a25", "25"),
            ("dup", "dep", "1.0"),
        ]
        .into_iter()
        .map(|(g, a, v)| (g.to_string(), a.to_string(), Some(v.to_string())))
        .collect();

        assert_eq!(
            tuples.len(),
            got.len(),
            "expected dependency extraction to not emit duplicates"
        );
        assert_eq!(got, expected);

        // Scope mapping is best-effort. If a dependency is declared in multiple configurations,
        // we keep a single deterministic scope for the coordinates.
        let expected_scopes = [
            (
                (String::from("g1"), String::from("a1"), Some("1".into())),
                "compile",
            ),
            (
                (String::from("g2"), String::from("a2"), Some("2".into())),
                "compile",
            ),
            (
                (String::from("g3"), String::from("a3"), Some("3".into())),
                "provided",
            ),
            (
                (String::from("g4"), String::from("a4"), Some("4".into())),
                "runtime",
            ),
            (
                (String::from("g5"), String::from("a5"), Some("5".into())),
                "test",
            ),
            (
                (String::from("g6"), String::from("a6"), Some("6".into())),
                "test",
            ),
            (
                (String::from("g7"), String::from("a7"), Some("7".into())),
                "test",
            ),
            (
                (String::from("g8"), String::from("a8"), Some("8".into())),
                "annotationProcessor",
            ),
            (
                (String::from("g9"), String::from("a9"), Some("9".into())),
                "annotationProcessor",
            ),
            (
                (String::from("g10"), String::from("a10"), Some("10".into())),
                "annotationProcessor",
            ),
            (
                (String::from("g11"), String::from("a11"), Some("11".into())),
                "annotationProcessor",
            ),
            (
                (String::from("g12"), String::from("a12"), Some("12".into())),
                "compile",
            ),
            (
                (String::from("g13"), String::from("a13"), Some("13".into())),
                "annotationProcessor",
            ),
            (
                (String::from("g14"), String::from("a14"), Some("14".into())),
                "compile",
            ),
            (
                (String::from("g15"), String::from("a15"), Some("15".into())),
                "runtime",
            ),
            (
                (String::from("g16"), String::from("a16"), Some("16".into())),
                "test",
            ),
            (
                (String::from("g17"), String::from("a17"), Some("17".into())),
                "test",
            ),
            (
                (String::from("g18"), String::from("a18"), Some("18".into())),
                "provided",
            ),
            (
                (String::from("g19"), String::from("a19"), Some("19".into())),
                "runtime",
            ),
            (
                (String::from("g20"), String::from("a20"), Some("20".into())),
                "provided",
            ),
            (
                (String::from("g21"), String::from("a21"), Some("21".into())),
                "annotationProcessor",
            ),
            (
                (String::from("g22"), String::from("a22"), Some("22".into())),
                "annotationProcessor",
            ),
            (
                (String::from("g23"), String::from("a23"), Some("23".into())),
                "provided",
            ),
            (
                (String::from("g24"), String::from("a24"), Some("24".into())),
                "annotationProcessor",
            ),
            (
                (String::from("g25"), String::from("a25"), Some("25".into())),
                "annotationProcessor",
            ),
            (
                (String::from("dup"), String::from("dep"), Some("1.0".into())),
                "compile",
            ),
        ];

        for (key, scope) in expected_scopes {
            assert_eq!(scopes.get(&key), Some(&Some(scope.to_string())));
        }
    }

    #[test]
    fn gradle_dependency_scope_merges_compile_only_and_runtime_only_to_compile() {
        let build_script = r#"
dependencies {
    compileOnly("g:a:1")
    runtimeOnly("g:a:1")
}
"#;
        let gradle_properties = GradleProperties::new();
        let deps = parse_gradle_dependencies_from_text(build_script, None, &gradle_properties);

        assert_eq!(
            deps.len(),
            1,
            "expected duplicate coordinates to be collapsed"
        );
        assert_eq!(deps[0].group_id, "g");
        assert_eq!(deps[0].artifact_id, "a");
        assert_eq!(deps[0].version.as_deref(), Some("1"));
        assert_eq!(
            deps[0].scope.as_deref(),
            Some("compile"),
            "a dependency needed on both compile and runtime classpaths should collapse to `compile`"
        );
    }

    #[test]
    fn parses_gradle_project_dependencies_from_text_supported_patterns() {
        let build_script = r#"
dependencies {
    implementation project(':lib')
    implementation(project(":lib2"))

    // Groovy map notation.
    implementation project(path: ':lib3')
    implementation project path: ':lib4'

    // Kotlin named args.
    testImplementation(project(path = ":lib5"))
    testImplementation(project(path = ":lib6", configuration = "default"))

    // No-parens Groovy call.
    testImplementation project ":lib7"

    // Comment stripping should prevent false positives.
    // implementation project(':ignored')
    /* implementation(project(":ignored2")) */
}
"#;

        let stripped = strip_gradle_comments(build_script);
        let deps = parse_gradle_project_dependencies_from_text(&stripped);
        let got: BTreeSet<_> = deps.into_iter().collect();

        let expected: BTreeSet<String> =
            [":lib", ":lib2", ":lib3", ":lib4", ":lib5", ":lib6", ":lib7"]
                .into_iter()
                .map(str::to_string)
                .collect();

        assert_eq!(got, expected);
    }

    #[test]
    fn parses_gradle_project_dependencies_ignores_project_refs_inside_slashy_strings() {
        let build_script = r#"
def ignored = /implementation project(':ignored')/
def ignored2 = $/implementation(project(path = ":ignored2"))/$

dependencies {
  implementation project(":real")
}
"#;

        let stripped = strip_gradle_comments(build_script);
        let deps = parse_gradle_project_dependencies_from_text(&stripped);
        let got: BTreeSet<_> = deps.into_iter().collect();

        let expected: BTreeSet<String> = [":real"].into_iter().map(str::to_string).collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn parses_gradle_project_dependencies_ignores_constraints_blocks() {
        let build_script = r#"
dependencies {
  constraints {
    implementation project(":ignored")
  }
  implementation project(":real")
}
"#;

        let stripped = strip_gradle_comments(build_script);
        let deps = parse_gradle_project_dependencies_from_text(&stripped);
        let got: BTreeSet<_> = deps.into_iter().collect();

        let expected: BTreeSet<String> = [":real"].into_iter().map(str::to_string).collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn parses_gradle_dependencies_from_text_version_catalog_bracket_notation() {
        let gradle_properties = GradleProperties::new();

        let catalog_toml = r#"
[versions]
guava = "32.0.0"
junit = "4.13.2"

[libraries]
foo-bar = { module = "com.example:foo-bar", version = "1.0.0" }
guava = { module = "com.google.guava:guava", version = { ref = "guava" } }
junit = { module = "junit:junit", version = { ref = "junit" } }

[bundles]
        test = ["junit", "guava"]
"#;
        let catalog = parse_gradle_version_catalog_from_toml(catalog_toml, &gradle_properties)
            .expect("parse catalog");

        let build_script = r#"
dependencies {
    implementation(libs["foo-bar"].get())
    testImplementation(libs.bundles["test"].get())
 }
"#;

        let deps =
            parse_gradle_dependencies_from_text(build_script, Some(&catalog), &gradle_properties);
        let got: BTreeSet<_> = deps
            .into_iter()
            .map(|d| (d.group_id, d.artifact_id, d.version, d.scope))
            .collect();

        assert!(got.contains(&(
            "com.example".to_string(),
            "foo-bar".to_string(),
            Some("1.0.0".to_string()),
            Some("compile".to_string())
        )));
        assert!(got.contains(&(
            "junit".to_string(),
            "junit".to_string(),
            Some("4.13.2".to_string()),
            Some("test".to_string())
        )));
        assert!(got.contains(&(
            "com.google.guava".to_string(),
            "guava".to_string(),
            Some("32.0.0".to_string()),
            Some("test".to_string())
        )));
    }

    #[test]
    fn parses_gradle_dependencies_from_text_version_catalog_wrappers_and_no_parens() {
        let gradle_properties = GradleProperties::new();

        let catalog_toml = r#"
[versions]
guava = "32.0.0"
junit = "4.13.2"

[libraries]
foo-bar = { module = "com.example:foo-bar", version = "1.0.0" }
guava = { module = "com.google.guava:guava", version = { ref = "guava" } }
junit = { module = "junit:junit", version = { ref = "junit" } }

[bundles]
test = ["junit", "guava"]
"#;
        let catalog = parse_gradle_version_catalog_from_toml(catalog_toml, &gradle_properties)
            .expect("parse catalog");

        let build_script = r#"
dependencies {
  implementation platform(libs["foo-bar"].get())
  testImplementation(platform(libs.bundles.test))
  testImplementation enforcedPlatform(libs.guava)
  testImplementation platform libs["foo-bar"].get()
  testImplementation enforcedPlatform libs.bundles.test
}
"#;

        let deps =
            parse_gradle_dependencies_from_text(build_script, Some(&catalog), &gradle_properties);
        let got: BTreeSet<_> = deps
            .into_iter()
            .map(|d| (d.group_id, d.artifact_id, d.version, d.scope))
            .collect();

        assert!(got.contains(&(
            "com.example".to_string(),
            "foo-bar".to_string(),
            Some("1.0.0".to_string()),
            Some("compile".to_string())
        )));
        assert!(got.contains(&(
            "junit".to_string(),
            "junit".to_string(),
            Some("4.13.2".to_string()),
            Some("test".to_string())
        )));
        assert!(got.contains(&(
            "com.google.guava".to_string(),
            "guava".to_string(),
            Some("32.0.0".to_string()),
            Some("test".to_string())
        )));
    }

    #[test]
    fn strip_gradle_comments_preserves_triple_quoted_strings() {
        let script = r#"
val url = """https://example.com//not-a-comment"""
def other = '''http://example.com//also-not-a-comment'''

// this is a comment
/* block comment */
"#;

        let stripped = strip_gradle_comments(script);
        assert!(stripped.contains("https://example.com//not-a-comment"));
        assert!(stripped.contains("http://example.com//also-not-a-comment"));
        assert!(!stripped.contains("this is a comment"));
        assert!(!stripped.contains("block comment"));
    }

    #[test]
    fn strip_gradle_comments_preserves_dollar_slashy_strings() {
        let script = r#"
def url = $/https://example.com//not-a-comment/$
def other = $/http://example.com/*also-not-a-comment*/ok/$

// this is a comment
/* block comment */
"#;

        let stripped = strip_gradle_comments(script);
        assert!(stripped.contains("https://example.com//not-a-comment"));
        assert!(stripped.contains("http://example.com/*also-not-a-comment*/ok"));
        assert!(!stripped.contains("this is a comment"));
        assert!(!stripped.contains("block comment"));
    }

    #[test]
    fn strip_gradle_comments_preserves_slashy_strings() {
        let script = r#"
def url = /https:\/\/example.com\/\/not-a-comment/
def ignored = /includeBuild("../ignored") \/\/ not a comment/
def ignored2 = /includeBuild("../ignored2") \/\* not a comment \*\/ /

// this is a comment
/* block comment */
"#;

        let stripped = strip_gradle_comments(script);
        assert!(stripped.contains(r#"https:\/\/example.com\/\/not-a-comment"#));
        assert!(stripped.contains(r#"includeBuild("../ignored") \/\/ not a comment"#));
        assert!(stripped.contains(r#"includeBuild("../ignored2") \/\* not a comment \*\/ "#));
        assert!(!stripped.contains("this is a comment"));
        assert!(!stripped.contains("block comment"));
    }

    #[test]
    fn strip_gradle_comments_does_not_get_stuck_in_unterminated_slashy_string() {
        let script = r#"
def pattern = /unterminated
// should be stripped
"#;

        let stripped = strip_gradle_comments(script);
        assert!(stripped.contains("def pattern = /unterminated"));
        assert!(
            !stripped.contains("should be stripped"),
            "expected line comment to be stripped even after unterminated slashy start; got: {stripped:?}"
        );
    }

    #[test]
    fn extract_named_brace_blocks_ignores_triple_quoted_strings() {
        let script = r#"
val ignored = """subprojects { dependencies { implementation("ignored:dep:1") } }"""

subprojects {
  // The brace in this triple-quoted string should not affect brace matching.
  val braces = """{"""
  dependencies {
    implementation("real:dep:1")
  }
}
"#;

        let blocks = extract_named_brace_blocks(script, "subprojects");
        assert_eq!(blocks.len(), 1, "blocks: {blocks:?}");
        assert!(
            blocks[0].contains("real:dep:1"),
            "expected extracted block to contain real dep, got: {:?}",
            blocks[0]
        );
        assert!(
            !blocks[0].contains("ignored:dep:1"),
            "expected triple-quoted string contents to be ignored, got: {:?}",
            blocks[0]
        );
    }

    #[test]
    fn extract_named_brace_blocks_ignores_slashy_strings() {
        let script = r#"
def ignored = /subprojects { dependencies { implementation("ignored:dep:1") } }/
def ignored2 = $/subprojects { dependencies { implementation("ignored2:dep:1") } }/$

subprojects {
  // The brace in this slashy string should not affect brace matching.
  def braces = /{/
  dependencies {
    implementation("real:dep:1")
  }
}
"#;

        let blocks = extract_named_brace_blocks(script, "subprojects");
        assert_eq!(blocks.len(), 1, "blocks: {blocks:?}");
        assert!(
            blocks[0].contains("real:dep:1"),
            "expected extracted block to contain real dep, got: {:?}",
            blocks[0]
        );
        assert!(
            !blocks[0].contains("ignored:dep:1"),
            "expected slashy string contents to be ignored, got: {:?}",
            blocks[0]
        );
        assert!(
            !blocks[0].contains("ignored2:dep:1"),
            "expected dollar slashy string contents to be ignored, got: {:?}",
            blocks[0]
        );
    }

    #[test]
    fn parses_gradle_dependencies_ignores_commented_out_deps() {
        let build_script = r#"
dependencies {
  // implementation("commented:dep:1")
  /* runtimeOnly("commented:block:2") */
  implementation("real:dep:3")
 }
"#;

        let gradle_properties = GradleProperties::new();
        let deps = parse_gradle_dependencies_from_text(build_script, None, &gradle_properties);
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].group_id, "real");
        assert_eq!(deps[0].artifact_id, "dep");
        assert_eq!(deps[0].version.as_deref(), Some("3"));
        assert_eq!(deps[0].scope.as_deref(), Some("compile"));
    }

    #[test]
    fn default_gradle_user_home_expands_tilde_from_env_var() {
        let dir = tempdir().expect("tempdir");
        let home = dir.path().join("home");
        fs::create_dir_all(&home).expect("create home");

        let _lock = env_lock();
        let _home = EnvVarGuard::set_path("HOME", Some(&home));
        let _userprofile = EnvVarGuard::set_path("USERPROFILE", Some(&home));
        let _gradle_home = EnvVarGuard::set_str("GRADLE_USER_HOME", Some("~/.gradle-custom"));

        let resolved = default_gradle_user_home().expect("gradle user home");
        assert_eq!(resolved, home.join(".gradle-custom"));
    }

    #[test]
    fn default_gradle_user_home_expands_user_home_placeholder_from_env_var() {
        let dir = tempdir().expect("tempdir");
        let home = dir.path().join("home");
        fs::create_dir_all(&home).expect("create home");

        let _lock = env_lock();
        let _home = EnvVarGuard::set_path("HOME", Some(&home));
        let _userprofile = EnvVarGuard::set_path("USERPROFILE", Some(&home));
        let _gradle_home =
            EnvVarGuard::set_str("GRADLE_USER_HOME", Some("${user.home}/.gradle-custom"));

        let resolved = default_gradle_user_home().expect("gradle user home");
        assert_eq!(resolved, home.join(".gradle-custom"));
    }

    #[test]
    fn default_gradle_user_home_expands_env_placeholder_from_env_var() {
        let dir = tempdir().expect("tempdir");
        let home = dir.path().join("home");
        let repo_root = dir.path().join("gradle-cache-root");
        fs::create_dir_all(&home).expect("create home");
        fs::create_dir_all(&repo_root).expect("create repo root");

        let _lock = env_lock();
        let _home = EnvVarGuard::set_path("HOME", Some(&home));
        let _userprofile = EnvVarGuard::set_path("USERPROFILE", Some(&home));
        let _base = EnvVarGuard::set_path("NOVA_TEST_GRADLE_HOME", Some(&repo_root));
        let _gradle_home = EnvVarGuard::set_str(
            "GRADLE_USER_HOME",
            Some("${env.NOVA_TEST_GRADLE_HOME}/custom"),
        );

        let resolved = default_gradle_user_home().expect("gradle user home");
        assert_eq!(resolved, repo_root.join("custom"));
    }

    #[test]
    fn default_gradle_user_home_ignores_unknown_placeholders() {
        let dir = tempdir().expect("tempdir");
        let home = dir.path().join("home");
        fs::create_dir_all(&home).expect("create home");

        let _lock = env_lock();
        let _home = EnvVarGuard::set_path("HOME", Some(&home));
        let _userprofile = EnvVarGuard::set_path("USERPROFILE", Some(&home));
        let _gradle_home = EnvVarGuard::set_str("GRADLE_USER_HOME", Some("${unknown}/custom"));

        let resolved = default_gradle_user_home().expect("gradle user home");
        assert_eq!(resolved, home.join(".gradle"));
    }

    #[test]
    fn parses_gradle_dependencies_ignores_dependency_like_text_inside_strings() {
        let build_script = r#"
val ignored = "implementation 'ignored:dep:1'"
val ignored2 = '''implementation("ignored2:dep:2")'''
val ignored3 = """implementation 'ignored3:dep:3'"""

dependencies {
  implementation("real:dep:4")
}
"#;

        let gradle_properties = GradleProperties::new();
        let deps = parse_gradle_dependencies_from_text(build_script, None, &gradle_properties);
        assert_eq!(deps.len(), 1, "deps: {deps:?}");
        assert_eq!(deps[0].group_id, "real");
        assert_eq!(deps[0].artifact_id, "dep");
        assert_eq!(deps[0].version.as_deref(), Some("4"));
    }

    #[test]
    fn parses_gradle_dependencies_ignores_dependency_like_text_inside_slashy_strings() {
        let build_script = r#"
def ignored = /implementation("ignored:dep:1")/
def ignored2 = $/implementation("ignored2:dep:2")/$

dependencies {
  implementation("real:dep:3")
}
"#;

        let gradle_properties = GradleProperties::new();
        let deps = parse_gradle_dependencies_from_text(build_script, None, &gradle_properties);
        assert_eq!(deps.len(), 1, "deps: {deps:?}");
        assert_eq!(deps[0].group_id, "real");
        assert_eq!(deps[0].artifact_id, "dep");
        assert_eq!(deps[0].version.as_deref(), Some("3"));
    }

    #[test]
    fn parses_gradle_dependencies_ignores_version_catalog_refs_inside_strings() {
        let gradle_properties = GradleProperties::new();

        let catalog_toml = r#"
[versions]
foo = "1.0.0"
guava = "32.0.0"

[libraries]
foo = { module = "com.example:foo", version = { ref = "foo" } }
guava = { module = "com.google.guava:guava", version = { ref = "guava" } }
"#;
        let catalog = parse_gradle_version_catalog_from_toml(catalog_toml, &gradle_properties)
            .expect("parse catalog");

        let build_script = r#"
val ignored = "implementation(libs.foo)"

dependencies {
  implementation(libs.guava)
}
"#;

        let deps =
            parse_gradle_dependencies_from_text(build_script, Some(&catalog), &gradle_properties);
        assert_eq!(deps.len(), 1, "deps: {deps:?}");
        assert_eq!(deps[0].group_id, "com.google.guava");
        assert_eq!(deps[0].artifact_id, "guava");
        assert_eq!(deps[0].version.as_deref(), Some("32.0.0"));
    }

    #[test]
    fn extract_named_brace_blocks_handles_triple_quoted_strings_with_braces() {
        let script = r#"
subprojects {
  val json = """
    { "k": 1 }
  """

  dependencies {
    implementation("real:dep:1")
  }
}
"#;

        let subprojects_blocks = extract_named_brace_blocks(script, "subprojects");
        assert_eq!(subprojects_blocks.len(), 1);
        let deps_blocks = extract_named_brace_blocks(&subprojects_blocks[0], "dependencies");
        assert_eq!(deps_blocks.len(), 1);

        let gradle_properties = GradleProperties::new();
        let deps = parse_gradle_dependencies_from_text(&deps_blocks[0], None, &gradle_properties);
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].group_id, "real");
        assert_eq!(deps[0].artifact_id, "dep");
    }

    #[test]
    fn parse_gradle_local_classpath_entries_ignores_commented_out_file_tree() {
        let dir = tempdir().expect("tempdir");
        let module_root = dir.path();
        fs::create_dir_all(module_root.join("libs")).expect("create libs dir");
        let jar_path = module_root.join("libs").join("a.jar");
        fs::write(&jar_path, b"").expect("write jar");

        let commented = r#"
dependencies {
  // implementation fileTree(dir: 'libs', include: ['*.jar'])
}
"#;
        let entries = parse_gradle_local_classpath_entries_from_text(module_root, commented);
        assert!(
            entries.is_empty(),
            "expected commented-out fileTree to be ignored; got: {entries:?}"
        );

        let active = r#"
dependencies {
  implementation fileTree(dir: 'libs', include: ['*.jar'])
}
"#;
        let entries = parse_gradle_local_classpath_entries_from_text(module_root, active);
        assert!(
            entries
                .iter()
                .any(|entry| entry.kind == ClasspathEntryKind::Jar && entry.path == jar_path),
            "expected {jar_path:?} to be present; got: {entries:?}"
        );
    }

    #[test]
    fn parse_gradle_local_classpath_entries_ignores_file_tree_inside_slashy_strings() {
        let dir = tempdir().expect("tempdir");
        let module_root = dir.path();
        fs::create_dir_all(module_root.join("libs")).expect("create libs dir");
        let jar_path = module_root.join("libs").join("a.jar");
        fs::write(&jar_path, b"").expect("write jar");

        let script = r#"
def ignored = /fileTree(dir: 'libs', include: ['*.jar'])/
def ignored2 = $/fileTree(dir: "libs", include: ["*.jar"])/$
"#;

        let entries = parse_gradle_local_classpath_entries_from_text(module_root, script);
        assert!(
            entries.is_empty(),
            "expected fileTree inside slashy strings to be ignored; got: {entries:?}"
        );
    }

    #[test]
    fn parse_gradle_local_classpath_entries_handles_parens_inside_string_literals() {
        let dir = tempdir().expect("tempdir");
        let module_root = dir.path();
        fs::create_dir_all(module_root.join("libs")).expect("create libs dir");
        let jar_path = module_root.join("libs").join("a) b.jar");
        fs::write(&jar_path, b"").expect("write jar");

        let script = r#"
dependencies {
  implementation files('libs/a) b.jar')
}
"#;

        let entries = parse_gradle_local_classpath_entries_from_text(module_root, script);
        assert!(
            entries
                .iter()
                .any(|entry| entry.kind == ClasspathEntryKind::Jar && entry.path == jar_path),
            "expected {jar_path:?} to be present; got: {entries:?}"
        );
    }

    #[test]
    fn parse_gradle_local_classpath_entries_handles_parens_inside_file_tree_dir_string_literal() {
        let dir = tempdir().expect("tempdir");
        let module_root = dir.path();
        fs::create_dir_all(module_root.join("libs").join("a) b")).expect("create libs dir");
        let jar_path = module_root.join("libs").join("a) b").join("a.jar");
        fs::write(&jar_path, b"").expect("write jar");

        let script = r#"
dependencies {
  implementation fileTree(dir: "libs/a) b", include: ["*.jar"])
}
"#;

        let entries = parse_gradle_local_classpath_entries_from_text(module_root, script);
        assert!(
            entries
                .iter()
                .any(|entry| entry.kind == ClasspathEntryKind::Jar && entry.path == jar_path),
            "expected {jar_path:?} to be present; got: {entries:?}"
        );
    }

    #[test]
    fn parse_gradle_local_classpath_entries_ignores_files_outside_dependencies_block() {
        let dir = tempdir().expect("tempdir");
        let module_root = dir.path();
        fs::create_dir_all(module_root.join("libs")).expect("create libs dir");
        let jar_path = module_root.join("libs").join("a.jar");
        fs::write(&jar_path, b"").expect("write jar");

        let script = r#"
val ignored = files("libs/a.jar")

dependencies {
  // intentionally empty
}
"#;

        let entries = parse_gradle_local_classpath_entries_from_text(module_root, script);
        assert!(
            entries.is_empty(),
            "expected files() outside dependencies block to be ignored; got: {entries:?}"
        );
    }

    #[test]
    fn parse_gradle_local_classpath_entries_ignores_buildscript_classpath_files() {
        let dir = tempdir().expect("tempdir");
        let module_root = dir.path();
        fs::create_dir_all(module_root.join("libs")).expect("create libs dir");
        let jar_path = module_root.join("libs").join("a.jar");
        fs::write(&jar_path, b"").expect("write jar");

        let script = r#"
buildscript {
  dependencies {
    classpath files("libs/a.jar")
  }
}

dependencies {
  // intentionally empty
}
"#;

        let entries = parse_gradle_local_classpath_entries_from_text(module_root, script);
        assert!(
            entries.is_empty(),
            "expected buildscript classpath files() to be ignored; got: {entries:?}"
        );
    }
}
