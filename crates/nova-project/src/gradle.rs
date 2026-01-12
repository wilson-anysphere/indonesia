use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use sha2::{Digest, Sha256};
use toml::Value;
use walkdir::WalkDir;

use nova_build_model::{
    GradleSnapshotFile, GradleSnapshotJavaCompileConfig, GRADLE_SNAPSHOT_REL_PATH,
    GRADLE_SNAPSHOT_SCHEMA_VERSION,
};

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

fn maybe_insert_buildsrc_module_ref(module_refs: &mut Vec<GradleModuleRef>, workspace_root: &Path) {
    let buildsrc_root = workspace_root.join("buildSrc");
    if !buildsrc_root.is_dir() {
        return;
    }
    if !root_project_has_sources(&buildsrc_root) {
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
#[derive(Debug, Clone, PartialEq, Eq)]
struct BuildFileFingerprint {
    digest: String,
}

impl BuildFileFingerprint {
    fn from_files(project_root: &Path, mut files: Vec<PathBuf>) -> std::io::Result<Self> {
        files.sort();
        files.dedup();

        let mut hasher = Sha256::new();
        for path in files {
            let rel = path.strip_prefix(project_root).unwrap_or(&path);
            hasher.update(rel.to_string_lossy().as_bytes());
            hasher.update([0]);

            let bytes = std::fs::read(&path)?;
            hasher.update(&bytes);
            hasher.update([0]);
        }

        Ok(Self {
            digest: hex::encode(hasher.finalize()),
        })
    }
}

fn collect_gradle_build_files(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    collect_gradle_build_files_rec(root, root, &mut out)?;
    // Stable sort for hashing.
    out.sort_by(|a, b| {
        let ra = a.strip_prefix(root).unwrap_or(a);
        let rb = b.strip_prefix(root).unwrap_or(b);
        ra.cmp(rb)
    });
    out.dedup();
    Ok(out)
}

fn collect_gradle_build_files_rec(
    root: &Path,
    dir: &Path,
    out: &mut Vec<PathBuf>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();

        if path.is_dir() {
            // Avoid scanning huge non-source directories that commonly show up in mono-repos.
            // These trees can contain many files that look like build files but should not
            // influence Nova's build fingerprint (e.g. vendored JS dependencies).
            if file_name == "node_modules" {
                continue;
            }
            // Bazel output trees are typically created at the workspace root and can be enormous.
            // Skip any top-level `bazel-*` entries (`bazel-out`, `bazel-bin`, `bazel-testlogs`,
            // `bazel-<workspace>`, etc).
            if dir == root && file_name.starts_with("bazel-") {
                continue;
            }
            if file_name == ".git"
                || file_name == ".gradle"
                || file_name == "build"
                || file_name == "target"
                || file_name == ".nova"
                || file_name == ".idea"
            {
                continue;
            }
            collect_gradle_build_files_rec(root, &path, out)?;
            continue;
        }

        let name = file_name.as_ref();

        // Gradle dependency locking can change resolved classpaths without modifying any build
        // scripts, so include lockfiles in the fingerprint.
        //
        // Patterns:
        // - `gradle.lockfile` at any depth.
        // - `*.lockfile` under any `dependency-locks/` directory (covers Gradle's default
        //   `gradle/dependency-locks/` location).
        if name == "gradle.lockfile" {
            out.push(path);
            continue;
        }
        if name.ends_with(".lockfile")
            && path.parent().is_some_and(|parent| {
                parent.ancestors().any(|dir| {
                    dir.file_name()
                        .is_some_and(|name| name == "dependency-locks")
                })
            })
        {
            out.push(path);
            continue;
        }

        // Match `nova-build` build-file watcher semantics by including any
        // `build.gradle*` / `settings.gradle*` variants.
        if name.starts_with("build.gradle") || name.starts_with("settings.gradle") {
            out.push(path);
            continue;
        }

        // Applied Gradle script plugins can influence dependencies and tasks
        // without being named `build.gradle*` / `settings.gradle*`.
        if name.ends_with(".gradle") || name.ends_with(".gradle.kts") {
            out.push(path);
            continue;
        }

        // Gradle version catalogs can define dependency versions and thus affect resolved
        // classpaths. In addition to the default `gradle/libs.versions.toml`, Gradle supports
        // custom catalogs referenced from `settings.gradle*` (e.g. `gradle/foo.versions.toml`).
        //
        // Only include catalogs that are direct children of a directory named `gradle` to avoid
        // accidentally picking up unrelated `*.toml` files elsewhere in the repo (including under
        // `node_modules/`).
        if name.ends_with(".versions.toml")
            && path
                .parent()
                .and_then(|parent| parent.file_name())
                .is_some_and(|dir| dir == "gradle")
        {
            out.push(path);
            continue;
        }
        match name {
            "gradle.properties" => out.push(path),
            // Gradle version catalogs can define dependency versions and thus
            // affect resolved classpaths.
            "libs.versions.toml" => out.push(path),
            "gradlew" | "gradlew.bat" => {
                if path == root.join(name) {
                    out.push(path);
                }
            }
            "gradle-wrapper.properties" => {
                if path.ends_with(Path::new("gradle/wrapper/gradle-wrapper.properties")) {
                    out.push(path);
                }
            }
            "gradle-wrapper.jar" => {
                if path.ends_with(Path::new("gradle/wrapper/gradle-wrapper.jar")) {
                    out.push(path);
                }
            }
            _ => {}
        }
    }
    Ok(())
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
        .is_some_and(|ext| ext.eq_ignore_ascii_case("jar"))
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

    let mut module_refs = if let Some(settings_path) = settings_path.as_ref() {
        let contents =
            std::fs::read_to_string(&settings_path).map_err(|source| ProjectError::Io {
                path: settings_path.clone(),
                source,
            })?;
        parse_gradle_settings_projects(&contents)
    } else {
        vec![GradleModuleRef::root()]
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

    maybe_insert_buildsrc_module_ref(&mut module_refs, root);

    let snapshot = load_gradle_snapshot(root);
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
    let version_catalog = load_gradle_version_catalog(root);
    dependencies.extend(parse_gradle_root_dependencies(root));

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
        dependencies.extend(parse_gradle_dependencies(
            &module_root,
            version_catalog.as_ref(),
        ));

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
                .is_some_and(|ext| ext.eq_ignore_ascii_case("jar"))
            {
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
    module_path.extend(extra_module_path.drain(..));
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

    let mut module_refs = if let Some(settings_path) = settings_path.as_ref() {
        let contents =
            std::fs::read_to_string(&settings_path).map_err(|source| ProjectError::Io {
                path: settings_path.clone(),
                source,
            })?;
        parse_gradle_settings_projects(&contents)
    } else {
        vec![GradleModuleRef::root()]
    };

    // See `load_gradle_project`: include the root project as a module when it contains sources,
    // even if subprojects exist. Keep the root first for deterministic ordering.
    if settings_path.is_some()
        && root_project_has_sources(root)
        && !module_refs.iter().any(|m| m.dir_rel == ".")
    {
        module_refs.insert(0, GradleModuleRef::root());
    }

    maybe_insert_buildsrc_module_ref(&mut module_refs, root);

    let snapshot = load_gradle_snapshot(root);
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
    if let Some(snapshot) = snapshot.as_ref() {
        if let Some(java) = java_config_from_snapshot(snapshot) {
            root_java = java;
        }
    }

    // Best-effort Gradle cache resolution. This does not execute Gradle; it only
    // adds jars that already exist in the local Gradle cache.
    let gradle_user_home = options
        .gradle_user_home
        .clone()
        .or_else(default_gradle_user_home);
    let version_catalog = load_gradle_version_catalog(root);
    let root_dependencies = parse_gradle_root_dependencies(root);

    let module_root_by_project_path: BTreeMap<String, PathBuf> = module_refs
        .iter()
        .map(|module_ref| {
            let module_root = if module_ref.dir_rel == "." {
                root.to_path_buf()
            } else if let Some(dir) = snapshot_project_dirs.get(&module_ref.project_path) {
                dir.clone()
            } else {
                root.join(&module_ref.dir_rel)
            };
            let module_root = canonicalize_or_fallback(&module_root);
            (module_ref.project_path.clone(), module_root)
        })
        .collect();

    let mut module_configs = Vec::new();
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

        // Best-effort: add dependent module outputs for project(":...") dependencies.
        for dep_project_path in parse_gradle_project_dependencies(&module_root) {
            let Some(dep_module_root) = module_root_by_project_path.get(&dep_project_path) else {
                continue;
            };
            classpath.push(ClasspathEntry {
                kind: ClasspathEntryKind::Directory,
                path: dep_module_root.join("build/classes/java/main"),
            });
        }
        for entry in &options.classpath_overrides {
            classpath.push(ClasspathEntry {
                kind: if entry
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("jar"))
                {
                    ClasspathEntryKind::Jar
                } else {
                    ClasspathEntryKind::Directory
                },
                path: entry.clone(),
            });
        }
        let mut dependencies = parse_gradle_dependencies(&module_root, version_catalog.as_ref());
        dependencies.extend(root_dependencies.iter().cloned());

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
                .is_some_and(|ext| ext.eq_ignore_ascii_case("jar"))
            {
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
        root_java,
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
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r#"['"]([^'"]+)['"]"#).expect("valid regex"));

    re.captures_iter(text)
        .filter_map(|caps| caps.get(1).map(|m| m.as_str().to_string()))
        .collect()
}

fn strip_gradle_comments(contents: &str) -> String {
    // Best-effort comment stripping to avoid parsing commented-out `include`/`projectDir` lines.
    // This is intentionally conservative and only strips:
    // - `// ...` to end-of-line
    // - `/* ... */` block comments
    // while preserving quoted strings (`'...'` / `"..."` / `'''...'''` / `"""..."""`).
    let bytes = contents.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());

    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut in_triple_single = false;
    let mut in_triple_double = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;

    while i < bytes.len() {
        let b = bytes[i];

        if in_line_comment {
            if b == b'\n' {
                in_line_comment = false;
                out.push(b'\n');
            }
            i += 1;
            continue;
        }

        if in_block_comment {
            if b == b'*' && bytes.get(i + 1) == Some(&b'/') {
                in_block_comment = false;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }

        if in_triple_single {
            if bytes[i..].starts_with(b"'''") {
                out.extend_from_slice(b"'''");
                in_triple_single = false;
                i += 3;
                continue;
            }
            out.push(b);
            i += 1;
            continue;
        }

        if in_triple_double {
            if bytes[i..].starts_with(b"\"\"\"") {
                out.extend_from_slice(b"\"\"\"");
                in_triple_double = false;
                i += 3;
                continue;
            }
            out.push(b);
            i += 1;
            continue;
        }

        if in_single {
            out.push(b);
            if b == b'\\' {
                if let Some(next) = bytes.get(i + 1) {
                    out.push(*next);
                    i += 2;
                    continue;
                }
            } else if b == b'\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }

        if in_double {
            out.push(b);
            if b == b'\\' {
                if let Some(next) = bytes.get(i + 1) {
                    out.push(*next);
                    i += 2;
                    continue;
                }
            } else if b == b'"' {
                in_double = false;
            }
            i += 1;
            continue;
        }

        if b == b'/' && bytes.get(i + 1) == Some(&b'/') {
            in_line_comment = true;
            i += 2;
            continue;
        }

        if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            in_block_comment = true;
            i += 2;
            continue;
        }

        if bytes[i..].starts_with(b"'''") {
            in_triple_single = true;
            out.extend_from_slice(b"'''");
            i += 3;
            continue;
        }

        if bytes[i..].starts_with(b"\"\"\"") {
            in_triple_double = true;
            out.extend_from_slice(b"\"\"\"");
            i += 3;
            continue;
        }

        if b == b'\'' {
            in_single = true;
            out.push(b'\'');
            i += 1;
            continue;
        }

        if b == b'"' {
            in_double = true;
            out.push(b'"');
            i += 1;
            continue;
        }

        out.push(b);
        i += 1;
    }

    String::from_utf8(out).unwrap_or_else(|_| contents.to_string())
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

fn is_word_byte(b: u8) -> bool {
    // Keep semantics aligned with Regex `\b` for ASCII: alphanumeric + underscore.
    b.is_ascii_alphanumeric() || b == b'_'
}

fn find_keyword_outside_strings(contents: &str, keyword: &str) -> Vec<usize> {
    let bytes = contents.as_bytes();
    let kw = keyword.as_bytes();
    if kw.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::new();

    let mut in_single = false;
    let mut in_double = false;
    let mut in_triple_single = false;
    let mut in_triple_double = false;
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];

        if in_triple_single {
            if bytes[i..].starts_with(b"'''") {
                in_triple_single = false;
                i += 3;
                continue;
            }
            i += 1;
            continue;
        }

        if in_triple_double {
            if bytes[i..].starts_with(b"\"\"\"") {
                in_triple_double = false;
                i += 3;
                continue;
            }
            i += 1;
            continue;
        }

        if in_single {
            if b == b'\\' {
                i = (i + 2).min(bytes.len());
                continue;
            }
            if b == b'\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }

        if in_double {
            if b == b'\\' {
                i = (i + 2).min(bytes.len());
                continue;
            }
            if b == b'"' {
                in_double = false;
            }
            i += 1;
            continue;
        }

        if bytes[i..].starts_with(b"'''") {
            in_triple_single = true;
            i += 3;
            continue;
        }

        if bytes[i..].starts_with(b"\"\"\"") {
            in_triple_double = true;
            i += 3;
            continue;
        }

        if b == b'\'' {
            in_single = true;
            i += 1;
            continue;
        }

        if b == b'"' {
            in_double = true;
            i += 1;
            continue;
        }

        if bytes[i..].starts_with(kw) {
            let prev_is_word = i
                .checked_sub(1)
                .and_then(|idx| bytes.get(idx))
                .is_some_and(|b| is_word_byte(*b));
            let next_is_word = bytes.get(i + kw.len()).is_some_and(|b| is_word_byte(*b));
            if !prev_is_word && !next_is_word {
                out.push(i);
                i += kw.len();
                continue;
            }
        }

        i += 1;
    }

    out
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
            let name = raw
                .trim()
                .trim_start_matches(':')
                .replace(':', "/")
                .replace('\\', "/");
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
    let bytes = contents.as_bytes();
    if bytes.get(open_paren_index) != Some(&b'(') {
        return None;
    }

    let mut depth = 0usize;
    let mut in_single = false;
    let mut in_double = false;

    let mut i = open_paren_index;
    while i < bytes.len() {
        let b = bytes[i];

        if in_single {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == b'\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }

        if in_double {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == b'"' {
                in_double = false;
            }
            i += 1;
            continue;
        }

        match b {
            b'\'' => {
                in_single = true;
                i += 1;
            }
            b'"' => {
                in_double = true;
                i += 1;
            }
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                i += 1;
                if depth == 0 {
                    let args = &contents[open_paren_index + 1..i - 1];
                    return Some((args.to_string(), i));
                }
            }
            _ => i += 1,
        }
    }

    None
}

fn extract_unparenthesized_args_until_eol_or_continuation(contents: &str, start: usize) -> String {
    // Groovy allows method calls without parentheses:
    //   include ':app', ':lib'
    // and can span lines after commas:
    //   include ':app',
    //           ':lib'
    let len = contents.len();
    let mut cursor = start;

    loop {
        let rest = &contents[cursor..];
        let line_break = rest.find('\n').map(|off| cursor + off).unwrap_or(len);
        let line = &contents[cursor..line_break];
        if line.trim_end().ends_with(',') && line_break < len {
            cursor = line_break + 1;
            continue;
        }
        return contents[start..line_break].to_string();
    }
}

fn parse_gradle_settings_project_dir_overrides(contents: &str) -> BTreeMap<String, String> {
    // Common overrides:
    //   project(':app').projectDir = file('modules/app')
    //   project(':lib').projectDir = new File(settingsDir, 'modules/lib')
    //   project(":app").projectDir = file("modules/app") (Kotlin DSL)
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(
            r#"(?x)
                \bproject\s*\(\s*['"](?P<project>[^'"]+)['"]\s*\)
                \s*\.\s*projectDir\s*=\s*
                (?:
                    file\s*\(\s*['"](?P<file_dir>[^'"]+)['"]\s*\)
                  |
                    (?:new\s+)?(?:java\.io\.)?File\s*\(\s*settingsDir\s*,\s*['"](?P<settings_dir>[^'"]+)['"]\s*\)
                )
            "#,
        )
        .expect("valid regex")
    });

    let mut overrides = BTreeMap::new();
    for caps in re.captures_iter(contents) {
        let project_path = normalize_project_path(&caps["project"]);
        let dir_rel = caps
            .name("file_dir")
            .or_else(|| caps.name("settings_dir"))
            .map(|m| m.as_str())
            .and_then(normalize_dir_rel);
        let Some(dir_rel) = dir_rel else {
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
    // 3) Otherwise, return `None` (caller uses defaults).
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
        (None, None) => parse_java_toolchain_language_version(contents).map(|v| JavaConfig {
            source: v,
            target: v,
            enable_preview: false,
        }),
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

const GRADLE_DEPENDENCY_CONFIGS: &str = r"(?:implementation|api|compileOnly|runtimeOnly|testImplementation|testRuntimeOnly|testCompileOnly|annotationProcessor|testAnnotationProcessor|kapt|kaptTest)";

fn gradle_scope_from_configuration(configuration: &str) -> Option<&'static str> {
    match configuration.trim().to_ascii_lowercase().as_str() {
        // Tests.
        "testimplementation"
        | "testruntimeonly"
        | "testcompileonly"
        | "testannotationprocessor"
        | "kapttest" => Some("test"),

        // Main compile.
        "implementation" | "api" => Some("compile"),

        // Main runtime only.
        "runtimeonly" => Some("runtime"),

        // Compile-only / annotation processor dependencies.
        "compileonly" | "annotationprocessor" | "kapt" => Some("provided"),

        _ => None,
    }
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

fn load_gradle_version_catalog(workspace_root: &Path) -> Option<GradleVersionCatalog> {
    let catalog_path = workspace_root.join("gradle").join("libs.versions.toml");
    let contents = std::fs::read_to_string(catalog_path).ok()?;
    parse_gradle_version_catalog_from_toml(&contents)
}

fn parse_gradle_version_catalog_from_toml(contents: &str) -> Option<GradleVersionCatalog> {
    let root: Value = toml::from_str(contents).ok()?;
    let root = root.as_table()?;

    let mut catalog = GradleVersionCatalog::default();

    if let Some(versions) = root.get("versions").and_then(Value::as_table) {
        for (k, v) in versions {
            if let Some(v) = v.as_str() {
                catalog.versions.insert(k.to_string(), v.to_string());
            }
        }
    }

    if let Some(libraries) = root.get("libraries").and_then(Value::as_table) {
        for (alias, value) in libraries {
            if let Some(lib) = parse_gradle_version_catalog_library(value, &catalog.versions) {
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
) -> Option<GradleVersionCatalogLibrary> {
    match value {
        // Not part of the requirements, but cheap to support.
        Value::String(text) => {
            let (group_id, artifact_id, version) = parse_maybe_maven_coord(text)?;
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
                Some(Value::String(v)) => Some(v.to_string()),
                Some(Value::Table(version_table)) => version_table
                    .get("ref")
                    .and_then(Value::as_str)
                    .and_then(|alias| versions.get(alias))
                    .cloned(),
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
) -> Vec<Dependency> {
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
        ));
    }
    out
}

fn parse_gradle_root_dependencies(root: &Path) -> Vec<Dependency> {
    // Root build scripts in multi-project Gradle repos often declare shared dependencies via
    // `allprojects { dependencies { ... } }` or `subprojects { dependencies { ... } }`.
    //
    // Parse them separately so we still discover dependencies even when subproject build scripts
    // are minimal.
    let version_catalog = load_gradle_version_catalog(root);
    parse_gradle_dependencies(root, version_catalog.as_ref())
}

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
    for re in [re_parens, re_no_parens] {
        for caps in re.captures_iter(contents) {
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
    deps
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
    static FILES_RE: OnceLock<Regex> = OnceLock::new();
    static FILE_TREE_DIR_RE: OnceLock<Regex> = OnceLock::new();
    static FILE_TREE_POSITIONAL_RE: OnceLock<Regex> = OnceLock::new();
    static FILE_TREE_MAP_RE: OnceLock<Regex> = OnceLock::new();

    // Note: this intentionally keeps the matcher simple; Gradle scripts are not trivially
    // parseable without a real Groovy/Kotlin parser. We rely on "exists on disk" checks to
    // avoid false positives.
    let files_re = FILES_RE
        .get_or_init(|| Regex::new(r#"(?s)\bfiles\s*\((?P<args>.*?)\)"#).expect("valid regex"));
    let file_tree_dir_re = FILE_TREE_DIR_RE.get_or_init(|| {
        Regex::new(r#"(?s)\bfileTree\s*\(\s*[^)]*?\bdir\s*(?:[:=])\s*['"](?P<dir>[^'"]+)['"]"#)
            .expect("valid regex")
    });
    let file_tree_positional_re = FILE_TREE_POSITIONAL_RE.get_or_init(|| {
        Regex::new(r#"(?s)\bfileTree\s*\(\s*['"](?P<dir>[^'"]+)['"]"#).expect("valid regex")
    });
    let file_tree_map_re = FILE_TREE_MAP_RE.get_or_init(|| {
        // Kotlin DSL also supports `fileTree(mapOf("dir" to "libs", ...))` style configuration.
        // We only extract the `"dir" to "..."` value and then add all `*.jar` entries under it.
        Regex::new(
            r#"(?s)\bfileTree\s*\(\s*mapOf\s*\(\s*.*?['"]dir['"]\s*to\s*(?:file\s*\(\s*)?['"](?P<dir>[^'"]+)['"]"#,
        )
        .expect("valid regex")
    });

    let mut out = Vec::new();

    for caps in files_re.captures_iter(contents) {
        let Some(args) = caps.name("args").map(|m| m.as_str()) else {
            continue;
        };
        for raw in extract_quoted_strings(args) {
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
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("jar"))
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
                .is_some_and(|ext| ext.eq_ignore_ascii_case("jar"))
            {
                out.push(ClasspathEntry {
                    kind: ClasspathEntryKind::Jar,
                    path: path.to_path_buf(),
                });
            }
        }
    };

    for caps in file_tree_dir_re.captures_iter(contents) {
        if let Some(dir) = caps.name("dir").map(|m| m.as_str()) {
            add_file_tree_dir(dir);
        }
    }

    for caps in file_tree_positional_re.captures_iter(contents) {
        if let Some(dir) = caps.name("dir").map(|m| m.as_str()) {
            add_file_tree_dir(dir);
        }
    }

    for caps in file_tree_map_re.captures_iter(contents) {
        if let Some(dir) = caps.name("dir").map(|m| m.as_str()) {
            add_file_tree_dir(dir);
        }
    }

    out
}

fn parse_gradle_dependencies_from_text(
    contents: &str,
    version_catalog: Option<&GradleVersionCatalog>,
) -> Vec<Dependency> {
    // Strip comments before running dependency regexes so commented-out dependency lines don't
    // end up polluting the extracted dependency list.
    //
    // This is best-effort but preserves quoted strings, so typical Gradle/Maven coordinate literals
    // are unaffected.
    let contents = strip_gradle_comments(contents);
    let contents = contents.as_str();

    let mut deps = Vec::new();

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
            r#"(?i)\b(?P<config>{configs})\b\s*\(?\s*['"](?P<group>[^:'"]+):(?P<artifact>[^:'"]+)(?::(?P<version>[^'"]+))?['"]"#,
        ))
        .expect("valid regex")
    });

    for caps in re_gav.captures_iter(contents) {
        deps.push(Dependency {
            group_id: caps["group"].to_string(),
            artifact_id: caps["artifact"].to_string(),
            version: caps.name("version").map(|m| m.as_str().to_string()),
            scope: caps
                .name("config")
                .and_then(|m| gradle_scope_from_configuration(m.as_str()))
                .map(str::to_string),
            classifier: None,
            type_: None,
        });
    }

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
            r#"(?is)\b(?P<config>{configs})\b\s*\(?\s*group\s*[:=]\s*['"](?P<group>[^'"]+)['"]\s*,\s*(?:name|module)\s*[:=]\s*['"](?P<artifact>[^'"]+)['"](?:\s*,\s*version\s*[:=]\s*['"](?P<version>[^'"]+)['"])?"#,
        ))
        .expect("valid regex")
    });

    for caps in re_map.captures_iter(contents) {
        deps.push(Dependency {
            group_id: caps["group"].to_string(),
            artifact_id: caps["artifact"].to_string(),
            version: caps.name("version").map(|m| m.as_str().to_string()),
            scope: caps
                .name("config")
                .and_then(|m| gradle_scope_from_configuration(m.as_str()))
                .map(str::to_string),
            classifier: None,
            type_: None,
        });
    }

    // Version catalog references (`implementation(libs.foo)` / `implementation libs.foo`).
    if let Some(version_catalog) = version_catalog {
        deps.extend(resolve_version_catalog_dependencies(
            contents,
            version_catalog,
        ));
    }
    sort_dedup_dependencies(&mut deps);
    deps
}

fn resolve_version_catalog_dependencies(
    contents: &str,
    version_catalog: &GradleVersionCatalog,
) -> Vec<Dependency> {
    static RE_DOT: OnceLock<Regex> = OnceLock::new();
    static RE_BRACKET: OnceLock<Regex> = OnceLock::new();
    static RE_BUNDLE_BRACKET: OnceLock<Regex> = OnceLock::new();

    let re_dot = RE_DOT.get_or_init(|| {
        let configs = GRADLE_DEPENDENCY_CONFIGS;
        Regex::new(&format!(
            r#"(?i)\b(?P<config>{configs})\s*\(?\s*libs\.(?P<ref>[A-Za-z0-9_]+(?:\.[A-Za-z0-9_]+)*)(?:\.get\(\))?\s*(?:\)|\s|$)"#,
        ))
        .expect("valid regex")
    });
    let re_bracket = RE_BRACKET.get_or_init(|| {
        let configs = GRADLE_DEPENDENCY_CONFIGS;
        Regex::new(&format!(
            r#"(?i)\b(?P<config>{configs})\s*\(?\s*libs\s*\[\s*['"](?P<ref>[^'"]+)['"]\s*\](?:\.get\(\))?\s*(?:\)|\s|$)"#,
        ))
        .expect("valid regex")
    });
    let re_bundle_bracket = RE_BUNDLE_BRACKET.get_or_init(|| {
        let configs = GRADLE_DEPENDENCY_CONFIGS;
        Regex::new(&format!(
            r#"(?i)\b(?P<config>{configs})\s*\(?\s*libs\.bundles\s*\[\s*['"](?P<bundle>[^'"]+)['"]\s*\](?:\.get\(\))?\s*(?:\)|\s|$)"#,
        ))
        .expect("valid regex")
    });

    let mut deps = Vec::new();
    for caps in re_dot.captures_iter(contents) {
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
        deps.extend(resolved);
    }

    for caps in re_bracket.captures_iter(contents) {
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
        deps.extend(resolved);
    }

    for caps in re_bundle_bracket.captures_iter(contents) {
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
    let version = lib.version.clone()?;
    Some(Dependency {
        group_id: lib.group_id.clone(),
        artifact_id: lib.artifact_id.clone(),
        version: Some(version),
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
    if let Some(home) = std::env::var_os("GRADLE_USER_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(home));
    }

    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)?;
    Some(home.join(".gradle"))
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

    let prefix = format!("{}-{}", dep.artifact_id, version);

    let mut preferred = Vec::new();
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

        if file_name.starts_with(&prefix) {
            preferred.push(path);
        } else {
            others.push(path);
        }
    }

    let mut out = if !preferred.is_empty() {
        preferred
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
    deps.dedup_by(|a, b| {
        let is_same_dep = a.group_id == b.group_id
            && a.artifact_id == b.artifact_id
            && a.version == b.version
            && a.classifier == b.classifier
            && a.type_ == b.type_;
        if !is_same_dep {
            return false;
        }

        // `Vec::dedup_by` keeps the *second* element passed to the predicate (`b`) and removes the
        // first (`a`). Merge scope information into `b` so we keep the most useful value.
        b.scope = merge_maven_like_scopes(a.scope.as_deref(), b.scope.as_deref());
        true
    });
}

/// Merge two Maven-like scopes (`compile`, `runtime`, `provided`, `test`) by choosing the most
/// permissive single scope that best approximates the union.
///
/// This is used to keep Gradle dependency lists stable and avoid duplicates when a dependency is
/// declared in multiple configurations (e.g. `testImplementation` + `implementation`).
fn merge_maven_like_scopes(a: Option<&str>, b: Option<&str>) -> Option<String> {
    // Prefer known scopes, but preserve unknown values if we have nothing better.
    let (a_known, a_unknown) = split_known_scope(a);
    let (b_known, b_unknown) = split_known_scope(b);

    if a_known.is_none() && b_known.is_none() {
        return a_unknown
            .or(b_unknown)
            .map(|s| s.to_string())
            .or_else(|| a.map(|s| s.to_string()))
            .or_else(|| b.map(|s| s.to_string()));
    }

    let mut compile = false;
    let mut runtime = false;
    let mut test = false;
    for scope in [a_known, b_known].into_iter().flatten() {
        match scope {
            "compile" => {
                compile = true;
                runtime = true;
            }
            "runtime" => runtime = true,
            "provided" => compile = true,
            "test" => test = true,
            _ => {}
        }
    }

    let merged = if compile && runtime {
        "compile"
    } else if runtime {
        "runtime"
    } else if compile {
        "provided"
    } else if test {
        "test"
    } else {
        // Should be unreachable when we have at least one known scope, but keep it safe.
        return a_known
            .or(b_known)
            .map(|s| s.to_string())
            .or_else(|| a_unknown.or(b_unknown).map(|s| s.to_string()));
    };

    Some(merged.to_string())
}

fn split_known_scope(scope: Option<&str>) -> (Option<&str>, Option<&str>) {
    match scope {
        Some("compile" | "runtime" | "provided" | "test") => (scope, None),
        Some(s) => (None, Some(s)),
        None => (None, None),
    }
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

    use super::{
        parse_gradle_dependencies_from_text, parse_gradle_project_dependencies_from_text,
        parse_gradle_settings_projects, parse_gradle_version_catalog_from_toml,
        sort_dedup_dependencies, strip_gradle_comments,
    };

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

    // Map notation with double quotes.
    testImplementation group: "org.example", name: "bar", version: "4.5.6"

    // Kotlin named args (even in a Groovy file, this is just text for regex extraction).
    implementation(group = "org.example", name = "baz", version = "7.8.9")

    // Map notation with trailing closure and parens (config covered by Task 72).
    annotationProcessor(group: 'com.google.auto.service', name: 'auto-service', version: '1.1.1') {
        // closure content shouldn't matter for extraction
        transitive = false
    }
}
"#;

        let mut deps = parse_gradle_dependencies_from_text(script, None);
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
            "bar".to_string(),
            Some("4.5.6".to_string())
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
    fn parses_gradle_dependencies_from_text_ignores_commented_out_dependencies() {
        let script = r#"
dependencies {
    // implementation "com.example:ignored:1"
    /* testImplementation("com.example:ignored2:2") */
    implementation("com.example:kept:3")
}
"#;

        let deps = parse_gradle_dependencies_from_text(script, None);
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
    fn parses_gradle_dependencies_from_text_supported_configurations_and_dedups() {
        let build_script = r#"
plugins {
    kotlin("jvm") version "1.9.0"
}

dependencies {
    implementation("g1:a1:1")
    api("g2:a2:2")
    compileOnly("g3:a3:3")
    runtimeOnly("g4:a4:4")
    testImplementation("g5:a5:5")
    testRuntimeOnly("g6:a6:6")
    testCompileOnly("g7:a7:7")
    annotationProcessor("g8:a8:8")
    testAnnotationProcessor("g9:a9:9")
    kapt("g10:a10:10")
    kaptTest("g11:a11:11")

    // Groovy-style call form (no parens)
    implementation 'g12:a12:12'
    kapt 'g13:a13:13'

    // Duplicate coordinates should not produce duplicates in output.
    implementation("dup:dep:1.0")
    testImplementation("dup:dep:1.0")
}
"#;

        let deps = parse_gradle_dependencies_from_text(build_script, None);

        let mut tuples: Vec<(String, String, Option<String>)> = Vec::new();
        let mut scopes: BTreeMap<(String, String, Option<String>), Option<String>> =
            BTreeMap::new();
        for dep in deps {
            tuples.push((
                dep.group_id.clone(),
                dep.artifact_id.clone(),
                dep.version.clone(),
            ));
            scopes.insert((dep.group_id, dep.artifact_id, dep.version), dep.scope);
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
        // we keep the most permissive Maven-like scope (union).
        let expected_scopes: [((String, String, Option<String>), &str); 14] = [
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
                "provided",
            ),
            (
                (String::from("g9"), String::from("a9"), Some("9".into())),
                "test",
            ),
            (
                (String::from("g10"), String::from("a10"), Some("10".into())),
                "provided",
            ),
            (
                (String::from("g11"), String::from("a11"), Some("11".into())),
                "test",
            ),
            (
                (String::from("g12"), String::from("a12"), Some("12".into())),
                "compile",
            ),
            (
                (String::from("g13"), String::from("a13"), Some("13".into())),
                "provided",
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
    fn parses_gradle_dependencies_from_text_version_catalog_bracket_notation() {
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
        let catalog = parse_gradle_version_catalog_from_toml(catalog_toml).expect("parse catalog");

        let build_script = r#"
dependencies {
    implementation(libs["foo-bar"].get())
    testImplementation(libs.bundles["test"].get())
}
"#;

        let deps = parse_gradle_dependencies_from_text(build_script, Some(&catalog));
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
}
