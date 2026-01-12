use crate::cache::{BuildCache, BuildFileFingerprint, CachedProjectInfo};
use crate::command::format_command;
use crate::jpms::{
    compiler_args_looks_like_jpms, infer_module_path_entries, main_source_roots_have_module_info,
};
use crate::{
    BuildError, BuildResult, BuildSystemKind, Classpath, CommandOutput, CommandRunner,
    DefaultCommandRunner, GradleBuildTask, JavaCompileConfig, Result,
};
use nova_build_model::{
    AnnotationProcessing, AnnotationProcessingConfig, GradleSnapshotFile,
    GradleSnapshotJavaCompileConfig, GradleSnapshotProject, GRADLE_SNAPSHOT_REL_PATH,
    GRADLE_SNAPSHOT_SCHEMA_VERSION,
};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

const NOVA_JSON_BEGIN: &str = "NOVA_JSON_BEGIN";
const NOVA_JSON_END: &str = "NOVA_JSON_END";
const NOVA_GRADLE_TASK: &str = "printNovaJavaCompileConfig";

/// Gradle's special `buildSrc/` build is modeled in `nova-project` as a synthetic
/// project with Gradle path `:__buildSrc`. It is not a real subproject of the
/// root build, so tasks cannot be invoked as `:__buildSrc:<task>`.
///
/// Instead, we must run Gradle against the nested build by passing
/// `--project-dir buildSrc` and invoking the Nova tasks at the root of that build.
const GRADLE_BUILDSRC_PROJECT_PATH: &str = ":__buildSrc";
const GRADLE_BUILDSRC_PROJECT_DIR: &str = "buildSrc";

fn gradle_buildsrc_suffix(project_path: &str) -> Option<&str> {
    let rest = project_path.strip_prefix(GRADLE_BUILDSRC_PROJECT_PATH)?;
    if rest.is_empty() || rest.starts_with(':') {
        Some(rest)
    } else {
        None
    }
}

const NOVA_ALL_JSON_BEGIN: &str = "NOVA_ALL_JSON_BEGIN";
const NOVA_ALL_JSON_END: &str = "NOVA_ALL_JSON_END";
const NOVA_GRADLE_ALL_TASK: &str = "printNovaAllJavaCompileConfigs";

const NOVA_APT_BEGIN: &str = "NOVA_APT_BEGIN";
const NOVA_APT_END: &str = "NOVA_APT_END";
const NOVA_GRADLE_APT_TASK: &str = "printNovaAnnotationProcessing";

const NOVA_PROJECTS_BEGIN: &str = "NOVA_PROJECTS_BEGIN";
const NOVA_PROJECTS_END: &str = "NOVA_PROJECTS_END";

#[derive(Debug, Clone)]
pub struct GradleConfig {
    /// Path to the `gradle` executable used when a project wrapper (`gradlew`)
    /// is not present.
    pub gradle_path: PathBuf,
    /// Prefer using the Gradle wrapper (`./gradlew`) when present.
    pub prefer_wrapper: bool,
}

impl Default for GradleConfig {
    fn default() -> Self {
        Self {
            gradle_path: PathBuf::from("gradle"),
            prefer_wrapper: true,
        }
    }
}

#[derive(Debug)]
pub struct GradleBuild {
    config: GradleConfig,
    runner: Arc<dyn CommandRunner>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GradleProjectInfo {
    pub path: String,
    pub dir: PathBuf,
}

impl GradleBuild {
    pub fn new(config: GradleConfig) -> Self {
        Self::with_runner(config, Arc::new(DefaultCommandRunner::default()))
    }

    pub fn with_runner(config: GradleConfig, runner: Arc<dyn CommandRunner>) -> Self {
        Self { config, runner }
    }

    pub fn projects(
        &self,
        project_root: &Path,
        cache: &BuildCache,
    ) -> Result<Vec<GradleProjectInfo>> {
        let fingerprint = gradle_build_fingerprint(project_root)?;

        if let Some(cached) = cache.load(project_root, BuildSystemKind::Gradle, &fingerprint)? {
            if let Some(projects) = cached.projects {
                let projects = projects
                    .into_iter()
                    .map(|p| GradleProjectInfo {
                        path: p.path,
                        dir: p.dir,
                    })
                    .collect::<Vec<_>>();

                // Best-effort: if the snapshot is missing/stale, repopulate it from cached data so
                // `nova-project` can still consume `.nova/queries/gradle.json` even when Gradle
                // queries are served entirely from Nova's build cache.
                let should_update_snapshot =
                    match read_gradle_snapshot_file(&gradle_snapshot_path(project_root)) {
                        Some(snapshot)
                            if snapshot.schema_version == GRADLE_SNAPSHOT_SCHEMA_VERSION
                                && snapshot.build_fingerprint == fingerprint.digest =>
                        {
                            let project_map: HashMap<String, PathBuf> = snapshot
                                .projects
                                .iter()
                                .map(|p| (p.path.clone(), p.project_dir.clone()))
                                .collect();
                            projects
                                .iter()
                                .any(|p| project_map.get(&p.path) != Some(&p.dir))
                        }
                        _ => true,
                    };
                if should_update_snapshot {
                    let _ = update_gradle_snapshot_projects(project_root, &fingerprint, &projects);
                }

                return Ok(projects);
            }
        }

        let (program, args, output) = self.run_print_projects(project_root)?;
        if !output.status.success() {
            return Err(BuildError::CommandFailed {
                tool: "gradle",
                command: format_command(&program, &args),
                code: output.status.code(),
                stdout: output.stdout,
                stderr: output.stderr,
                output_truncated: output.truncated,
            });
        }

        let combined = output.combined();
        let projects = parse_gradle_projects_output(&combined)?;

        let cached_projects: Vec<CachedProjectInfo> = projects
            .iter()
            .map(|p| CachedProjectInfo {
                path: p.path.clone(),
                dir: p.dir.clone(),
            })
            .collect();

        let mut data = cache
            .load(project_root, BuildSystemKind::Gradle, &fingerprint)?
            .unwrap_or_default();
        data.projects = Some(cached_projects);

        // Best-effort: persist a workspace-local Gradle model snapshot so `nova-project`
        // can reuse the richer projectDir mapping without invoking Gradle.
        let _ = update_gradle_snapshot_projects(project_root, &fingerprint, &projects);

        // Prefer writing the snapshot even if updating Nova's build cache fails; the snapshot is a
        // workspace-local handoff consumed by `nova-project` and is independent of the cache.
        let store_result = cache.store(project_root, BuildSystemKind::Gradle, &fingerprint, &data);
        store_result?;

        Ok(projects)
    }

    pub fn classpath(
        &self,
        project_root: &Path,
        project_path: Option<&str>,
        cache: &BuildCache,
    ) -> Result<Classpath> {
        let project_path = project_path.filter(|p| *p != ":");
        let cfg = self.java_compile_config(project_root, project_path, cache)?;
        Ok(Classpath::new(cfg.compile_classpath))
    }

    /// Fetch Java compile configs for all Gradle subprojects in a single Gradle invocation.
    ///
    /// On success, this also populates the build cache for each project path so subsequent
    /// per-module `java_compile_config(project_path=Some(..))` calls become cache hits.
    pub fn java_compile_configs_all(
        &self,
        project_root: &Path,
        cache: &BuildCache,
    ) -> Result<Vec<(String, JavaCompileConfig)>> {
        let fingerprint = gradle_build_fingerprint(project_root)?;

        // Best-effort cache hit: if we already have per-project configs for the current fingerprint,
        // avoid spawning Gradle again.
        //
        // This keeps `java_compile_configs_all` consistent with `java_compile_config`'s cache-hit
        // behavior and is useful for callers that periodically re-request full workspace configs.
        if let Some(data) = cache.load(project_root, BuildSystemKind::Gradle, &fingerprint)? {
            if let Some(projects) = data.projects.as_ref() {
                let mut out = Vec::new();
                let mut missing = false;

                for project in projects {
                    let cfg = if project.path == ":" {
                        data.modules
                            .get(":")
                            .and_then(|m| m.java_compile_config.clone())
                            // Backwards compat: older caches only stored the root config under
                            // `<root>`.
                            .or_else(|| {
                                data.modules
                                    .get("<root>")
                                    .and_then(|m| m.java_compile_config.clone())
                            })
                    } else {
                        data.modules
                            .get(&project.path)
                            .and_then(|m| m.java_compile_config.clone())
                    };

                    let Some(cfg) = cfg else {
                        missing = true;
                        break;
                    };

                    out.push((project.path.clone(), cfg));
                }

                if !missing {
                    // Best-effort: keep the snapshot available even when serving purely from
                    // cache (no Gradle invocation).
                    let should_update_snapshot =
                        match read_gradle_snapshot_file(&gradle_snapshot_path(project_root)) {
                            Some(snapshot)
                                if snapshot.schema_version == GRADLE_SNAPSHOT_SCHEMA_VERSION
                                    && snapshot.build_fingerprint == fingerprint.digest =>
                            {
                                let snapshot_project_map: HashMap<String, PathBuf> = snapshot
                                    .projects
                                    .iter()
                                    .map(|p| (p.path.clone(), p.project_dir.clone()))
                                    .collect();

                                let projects_mismatch = projects
                                    .iter()
                                    .any(|p| snapshot_project_map.get(&p.path) != Some(&p.dir));
                                let missing_configs = projects
                                    .iter()
                                    .any(|p| !snapshot.java_compile_configs.contains_key(&p.path));

                                projects_mismatch || missing_configs
                            }
                            _ => true,
                        };

                    if should_update_snapshot {
                        // Snapshot project list.
                        let snapshot_projects: Vec<GradleSnapshotProject> = projects
                            .iter()
                            .map(|p| GradleSnapshotProject {
                                path: p.path.clone(),
                                project_dir: p.dir.clone(),
                            })
                            .collect();

                        // Snapshot compile configs.
                        let project_dirs: HashMap<String, PathBuf> = projects
                            .iter()
                            .map(|p| (p.path.clone(), p.dir.clone()))
                            .collect();

                        let mut snapshot_configs: BTreeMap<
                            String,
                            GradleSnapshotJavaCompileConfig,
                        > = BTreeMap::new();
                        for (path, cfg) in &out {
                            let cfg_for_snapshot = if path == ":" {
                                // Prefer the cached `<root>` config for the snapshot's root entry:
                                // - for normal roots, `<root>` is the root project's own config
                                // - for aggregator roots, `<root>` is the union fallback
                                data.modules
                                    .get("<root>")
                                    .and_then(|m| m.java_compile_config.as_ref())
                                    .unwrap_or(cfg)
                            } else {
                                cfg
                            };

                            let project_dir = project_dirs
                                .get(path)
                                .cloned()
                                .unwrap_or_else(|| project_root.to_path_buf());
                            snapshot_configs.insert(
                                path.clone(),
                                GradleSnapshotJavaCompileConfig {
                                    project_dir,
                                    compile_classpath: cfg_for_snapshot.compile_classpath.clone(),
                                    test_classpath: cfg_for_snapshot.test_classpath.clone(),
                                    module_path: cfg_for_snapshot.module_path.clone(),
                                    main_source_roots: cfg_for_snapshot.main_source_roots.clone(),
                                    test_source_roots: cfg_for_snapshot.test_source_roots.clone(),
                                    main_output_dir: cfg_for_snapshot.main_output_dir.clone(),
                                    test_output_dir: cfg_for_snapshot.test_output_dir.clone(),
                                    source: cfg_for_snapshot.source.clone(),
                                    target: cfg_for_snapshot.target.clone(),
                                    release: cfg_for_snapshot.release.clone(),
                                    enable_preview: cfg_for_snapshot.enable_preview,
                                },
                            );
                        }

                        let _ = update_gradle_snapshot(project_root, &fingerprint, |snapshot| {
                            snapshot.projects = snapshot_projects;
                            snapshot.java_compile_configs = snapshot_configs;
                        });
                    }

                    out.sort_by(|a, b| a.0.cmp(&b.0));
                    out.dedup_by(|a, b| a.0 == b.0);
                    return Ok(out);
                }
            }
        }

        let (program, args, output) = self.run_print_all_java_compile_configs(project_root)?;
        if !output.status.success() {
            return Err(BuildError::CommandFailed {
                tool: "gradle",
                command: format_command(&program, &args),
                code: output.status.code(),
                stdout: output.stdout,
                stderr: output.stderr,
                output_truncated: output.truncated,
            });
        }

        let combined = output.combined();
        let parsed = parse_gradle_all_java_compile_configs_output(&combined)?;

        // Sort to keep results deterministic even if Gradle emits projects in a different order.
        let mut projects = parsed.projects;
        projects.sort_by(|a, b| a.path.cmp(&b.path));

        // Batch update the cache in a single write.
        let mut data = cache
            .load(project_root, BuildSystemKind::Gradle, &fingerprint)?
            .unwrap_or_default();

        // Also refresh cached project directories (used by other helpers).
        let mut cached_projects: Vec<CachedProjectInfo> = projects
            .iter()
            .map(|p| CachedProjectInfo {
                path: p.path.clone(),
                dir: PathBuf::from(p.project_dir.clone()),
            })
            .collect();
        cached_projects.sort_by(|a, b| a.path.cmp(&b.path));
        cached_projects.dedup_by(|a, b| a.path == b.path);
        data.projects = Some(cached_projects);

        // Snapshot project list for `.nova/queries/gradle.json` (best-effort). This snapshot is
        // consumed by `nova-project` to avoid invoking Gradle during discovery.
        let snapshot_projects: Vec<GradleSnapshotProject> = data
            .projects
            .as_ref()
            .map(|projects| {
                projects
                    .iter()
                    .map(|p| GradleSnapshotProject {
                        path: p.path.clone(),
                        project_dir: p.dir.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut snapshot_configs: BTreeMap<String, GradleSnapshotJavaCompileConfig> =
            BTreeMap::new();

        let mut out = Vec::new();
        let mut root_config = None::<JavaCompileConfig>;
        let mut root_missing_compile_classpath = false;
        let mut non_root_configs_for_union = Vec::new();
        for project in projects {
            let is_root = project.path == ":";
            // Remember whether the root project lacked a `compileClasspath` configuration
            // (common for aggregator roots).
            if is_root && project.config.compile_classpath.is_none() {
                root_missing_compile_classpath = true;
            }

            let project_dir = PathBuf::from(project.project_dir);
            let main_output_fallback = project_dir
                .join("build")
                .join("classes")
                .join("java")
                .join("main");
            let test_output_fallback = gradle_test_output_dir_from_main(&main_output_fallback);

            let mut config = normalize_gradle_java_compile_config(
                project.config,
                main_output_fallback,
                test_output_fallback,
            );
            augment_java_compile_config_with_conventional_gradle_source_roots(
                &project_dir,
                &mut config,
            );

            if is_root {
                root_config = Some(config.clone());

                // Cache the root project under `":"` so `java_compile_configs_all` can be served
                // from cache without re-running Gradle (and without losing the distinction between
                // the root project's own config vs the `<root>` union fallback for aggregator
                // builds).
                let module = data.modules.entry(":".to_string()).or_default();
                module.java_compile_config = Some(config.clone());
                module.classpath = Some(config.compile_classpath.clone());
            } else {
                let module = data.modules.entry(project.path.clone()).or_default();
                module.java_compile_config = Some(config.clone());
                // Keep populating the legacy classpath field for older readers.
                module.classpath = Some(config.compile_classpath.clone());
                non_root_configs_for_union.push(config.clone());
            }

            snapshot_configs.insert(
                project.path.clone(),
                GradleSnapshotJavaCompileConfig {
                    project_dir: project_dir.clone(),
                    compile_classpath: config.compile_classpath.clone(),
                    test_classpath: config.test_classpath.clone(),
                    module_path: config.module_path.clone(),
                    main_source_roots: config.main_source_roots.clone(),
                    test_source_roots: config.test_source_roots.clone(),
                    main_output_dir: config.main_output_dir.clone(),
                    test_output_dir: config.test_output_dir.clone(),
                    source: config.source.clone(),
                    target: config.target.clone(),
                    release: config.release.clone(),
                    enable_preview: config.enable_preview,
                },
            );

            out.push((project.path, config));
        }

        // Cache the root module under `<root>`:
        // - If the root is an aggregator (no compileClasspath), store the union of subprojects so
        //   subsequent `java_compile_config(project_path=None)` calls are cache hits and preserve
        //   the existing union fallback semantics.
        // - Otherwise, store the root project's own config.
        if root_missing_compile_classpath {
            let union = JavaCompileConfig::union(non_root_configs_for_union);
            let module = data.modules.entry("<root>".to_string()).or_default();
            module.java_compile_config = Some(union.clone());
            module.classpath = Some(union.compile_classpath.clone());

            // Best-effort: persist a root `":"` compile config entry even when the root project is
            // an aggregator (no Java plugin). This allows `nova-project` to reuse a workspace-level
            // classpath in `.nova/queries/gradle.json`.
            snapshot_configs.insert(
                ":".to_string(),
                GradleSnapshotJavaCompileConfig {
                    project_dir: project_root.to_path_buf(),
                    compile_classpath: union.compile_classpath.clone(),
                    test_classpath: union.test_classpath.clone(),
                    module_path: union.module_path.clone(),
                    main_source_roots: union.main_source_roots.clone(),
                    test_source_roots: union.test_source_roots.clone(),
                    main_output_dir: union.main_output_dir.clone(),
                    test_output_dir: union.test_output_dir.clone(),
                    source: union.source.clone(),
                    target: union.target.clone(),
                    release: union.release.clone(),
                    enable_preview: union.enable_preview,
                },
            );
        } else if let Some(root) = root_config {
            let module = data.modules.entry("<root>".to_string()).or_default();
            module.java_compile_config = Some(root.clone());
            module.classpath = Some(root.compile_classpath.clone());
        }

        // Best-effort: persist a workspace-local Gradle model snapshot so `nova-project` can reuse
        // these resolved configs without invoking Gradle during project discovery.
        let snapshot_result = update_gradle_snapshot(project_root, &fingerprint, |snapshot| {
            snapshot.projects = snapshot_projects;
            snapshot.java_compile_configs = snapshot_configs;
        });
        let store_result = cache.store(project_root, BuildSystemKind::Gradle, &fingerprint, &data);

        // Prefer writing the snapshot even if updating Nova's build cache fails; the snapshot is a
        // workspace-local handoff consumed by `nova-project` and is independent of the cache.
        let _ = snapshot_result;
        store_result?;

        out.sort_by(|a, b| a.0.cmp(&b.0));
        out.dedup_by(|a, b| a.0 == b.0);
        Ok(out)
    }

    pub fn java_compile_config(
        &self,
        project_root: &Path,
        project_path: Option<&str>,
        cache: &BuildCache,
    ) -> Result<JavaCompileConfig> {
        let project_path = project_path.filter(|p| *p != ":");
        let fingerprint = gradle_build_fingerprint(project_root)?;
        let module_key = project_path.unwrap_or("<root>");

        if let Some(cached) = cache.get_module(
            project_root,
            BuildSystemKind::Gradle,
            &fingerprint,
            module_key,
        )? {
            if let Some(cfg) = cached.java_compile_config {
                let project_path_for_snapshot = project_path.unwrap_or(":");
                let should_update_snapshot =
                    match read_gradle_snapshot_file(&gradle_snapshot_path(project_root)) {
                        Some(snapshot)
                            if snapshot.schema_version == GRADLE_SNAPSHOT_SCHEMA_VERSION
                                && snapshot.build_fingerprint == fingerprint.digest =>
                        {
                            !snapshot
                                .java_compile_configs
                                .contains_key(project_path_for_snapshot)
                        }
                        _ => true,
                    };
                if should_update_snapshot {
                    if let Ok(project_dir) = gradle_project_dir_cached(
                        project_root,
                        Some(project_path_for_snapshot),
                        cache,
                        &fingerprint,
                    ) {
                        let _ = update_gradle_snapshot_java_compile_config(
                            project_root,
                            &fingerprint,
                            project_path_for_snapshot,
                            &project_dir,
                            &cfg,
                        );
                    }
                }

                return Ok(cfg);
            }
            // Backwards-compat: older cache entries may contain only classpath.
            if let Some(entries) = cached.classpath {
                let cfg = JavaCompileConfig {
                    compile_classpath: entries,
                    ..JavaCompileConfig::default()
                };

                // Best-effort: older cache entries might not include a full `JavaCompileConfig`,
                // but still contain a useful classpath. Populate the Gradle snapshot so
                // `nova-project` can consume it even without invoking Gradle.
                let project_path_for_snapshot = project_path.unwrap_or(":");
                let should_update_snapshot =
                    match read_gradle_snapshot_file(&gradle_snapshot_path(project_root)) {
                        Some(snapshot)
                            if snapshot.schema_version == GRADLE_SNAPSHOT_SCHEMA_VERSION
                                && snapshot.build_fingerprint == fingerprint.digest =>
                        {
                            !snapshot
                                .java_compile_configs
                                .contains_key(project_path_for_snapshot)
                        }
                        _ => true,
                    };
                if should_update_snapshot {
                    if let Ok(project_dir) = gradle_project_dir_cached(
                        project_root,
                        Some(project_path_for_snapshot),
                        cache,
                        &fingerprint,
                    ) {
                        let _ = update_gradle_snapshot_java_compile_config(
                            project_root,
                            &fingerprint,
                            project_path_for_snapshot,
                            &project_dir,
                            &cfg,
                        );
                    }
                }

                return Ok(cfg);
            }
        }

        // Optimization: for multi-project builds, fetch all module configs up-front when a
        // workspace-level query is requested. This avoids first running the root-only task and
        // then falling back to the batch task for aggregator roots.
        if project_path.is_none()
            && gradle_settings_suggest_multi_project(project_root)
            && self.java_compile_configs_all(project_root, cache).is_ok()
        {
            if let Some(cached) = cache.get_module(
                project_root,
                BuildSystemKind::Gradle,
                &fingerprint,
                module_key,
            )? {
                if let Some(cfg) = cached.java_compile_config {
                    let project_path_for_snapshot = project_path.unwrap_or(":");
                    let should_update_snapshot =
                        match read_gradle_snapshot_file(&gradle_snapshot_path(project_root)) {
                            Some(snapshot)
                                if snapshot.schema_version == GRADLE_SNAPSHOT_SCHEMA_VERSION
                                    && snapshot.build_fingerprint == fingerprint.digest =>
                            {
                                !snapshot
                                    .java_compile_configs
                                    .contains_key(project_path_for_snapshot)
                            }
                            _ => true,
                        };
                    if should_update_snapshot {
                        if let Ok(project_dir) = gradle_project_dir_cached(
                            project_root,
                            Some(project_path_for_snapshot),
                            cache,
                            &fingerprint,
                        ) {
                            let _ = update_gradle_snapshot_java_compile_config(
                                project_root,
                                &fingerprint,
                                project_path_for_snapshot,
                                &project_dir,
                                &cfg,
                            );
                        }
                    }

                    return Ok(cfg);
                }
                if let Some(entries) = cached.classpath {
                    let cfg = JavaCompileConfig {
                        compile_classpath: entries,
                        ..JavaCompileConfig::default()
                    };

                    // Best-effort: persist a snapshot for legacy classpath-only cache entries.
                    let project_path_for_snapshot = project_path.unwrap_or(":");
                    let should_update_snapshot =
                        match read_gradle_snapshot_file(&gradle_snapshot_path(project_root)) {
                            Some(snapshot)
                                if snapshot.schema_version == GRADLE_SNAPSHOT_SCHEMA_VERSION
                                    && snapshot.build_fingerprint == fingerprint.digest =>
                            {
                                !snapshot
                                    .java_compile_configs
                                    .contains_key(project_path_for_snapshot)
                            }
                            _ => true,
                        };
                    if should_update_snapshot {
                        if let Ok(project_dir) = gradle_project_dir_cached(
                            project_root,
                            Some(project_path_for_snapshot),
                            cache,
                            &fingerprint,
                        ) {
                            let _ = update_gradle_snapshot_java_compile_config(
                                project_root,
                                &fingerprint,
                                project_path_for_snapshot,
                                &project_dir,
                                &cfg,
                            );
                        }
                    }

                    return Ok(cfg);
                }
            }
        }

        // Optimization: when a per-module config is requested and there's a cache miss, attempt to
        // fetch *all* project configs in a single Gradle invocation. This avoids N× Gradle
        // startups in multi-module workspaces when callers query multiple modules in sequence.
        if let Some(requested_path) = project_path {
            // `buildSrc/` is a separate nested build (invoked via `--project-dir buildSrc`), so it
            // can never be part of the root build's all-project query.
            if gradle_buildsrc_suffix(requested_path).is_some() {
                // Skip the batch helper task; we'll fall back to a targeted buildSrc query below.
            } else
            // Avoid running the batch task for simple single-project builds.
            if gradle_settings_suggest_multi_project(project_root) {
                if let Ok(configs) = self.java_compile_configs_all(project_root, cache) {
                    if let Some((_, cfg)) =
                        configs.into_iter().find(|(path, _)| path == requested_path)
                    {
                        return Ok(cfg);
                    }
                }
            }
        }

        let (program, args, output) =
            match self.run_print_java_compile_config(project_root, project_path) {
                Ok(output) => output,
                Err(BuildError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {
                    if let Some(cached) = cache.get_module_best_effort(
                        project_root,
                        BuildSystemKind::Gradle,
                        module_key,
                    )? {
                        if let Some(cfg) = cached.java_compile_config {
                            return Ok(cfg);
                        }
                        if let Some(entries) = cached.classpath {
                            return Ok(JavaCompileConfig {
                                compile_classpath: entries,
                                ..JavaCompileConfig::default()
                            });
                        }
                    }
                    return Err(BuildError::Io(err));
                }
                Err(err) => return Err(err),
            };
        if !output.status.success() {
            return Err(BuildError::CommandFailed {
                tool: "gradle",
                command: format_command(&program, &args),
                code: output.status.code(),
                stdout: output.stdout,
                stderr: output.stderr,
                output_truncated: output.truncated,
            });
        }

        let combined = output.combined();
        let json = parse_gradle_java_compile_config_json(&combined)?;
        let is_buildsrc = project_path.is_some_and(|p| gradle_buildsrc_suffix(p).is_some());
        let project_path_for_snapshot = if is_buildsrc {
            project_path
                .expect("buildSrc project path should be Some(..)")
                .to_string()
        } else {
            json.project_path
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .unwrap_or_else(|| project_path.unwrap_or(":").to_string())
        };

        // Aggregator roots often don't apply the Java plugin and thus do not
        // expose `compileClasspath`. When querying the workspace-level config
        // (project path == None), fall back to unioning all subprojects.
        if project_path.is_none() && json.compile_classpath.is_none() {
            // Prefer the batch helper task which fetches all subproject configs in a single
            // Gradle invocation.
            let union = match self.java_compile_configs_all(project_root, cache) {
                Ok(configs) => JavaCompileConfig::union(
                    configs
                        .into_iter()
                        .filter(|(path, _)| path != ":")
                        .map(|(_, cfg)| cfg),
                ),
                Err(_) => {
                    // Backwards compatibility: fall back to the older multi-invocation path.
                    let projects = self.projects(project_root, cache)?;

                    let mut configs = Vec::new();
                    for project in projects.into_iter().filter(|p| p.path != ":") {
                        configs.push(self.java_compile_config(
                            project_root,
                            Some(project.path.as_str()),
                            cache,
                        )?);
                    }
                    JavaCompileConfig::union(configs)
                }
            };

            cache.update_module(
                project_root,
                BuildSystemKind::Gradle,
                &fingerprint,
                module_key,
                |m| {
                    m.java_compile_config = Some(union.clone());
                    m.classpath = Some(union.compile_classpath.clone());
                },
            )?;

            // Best-effort: record the union config for the root project so `nova-project`
            // can at least reuse a workspace-level classpath without invoking Gradle.
            let _ = update_gradle_snapshot_java_compile_config(
                project_root,
                &fingerprint,
                ":",
                project_root,
                &union,
            );

            return Ok(union);
        }

        let project_dir_from_payload = json
            .project_dir
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);

        let main_output_fallback = match project_dir_from_payload.as_ref() {
            Some(dir) => dir.join("build").join("classes").join("java").join("main"),
            None => gradle_output_dir_cached(project_root, project_path, cache, &fingerprint)?,
        };
        let test_output_fallback = gradle_test_output_dir_from_main(&main_output_fallback);
        let project_dir = match project_dir_from_payload {
            Some(dir) => dir,
            None => gradle_project_dir_cached(project_root, project_path, cache, &fingerprint)?,
        };
        let mut config =
            normalize_gradle_java_compile_config(json, main_output_fallback, test_output_fallback);
        // Gradle config extraction only reports `sourceSets.main` / `sourceSets.test`, but
        // conventional layouts commonly add source sets like `integrationTest`.
        //
        // Always augment the config with conventional `src/<sourceSet>/java` roots so downstream
        // consumers relying on build-derived roots don't drop these source sets.
        augment_java_compile_config_with_conventional_gradle_source_roots(
            &project_dir,
            &mut config,
        );

        cache.update_module(
            project_root,
            BuildSystemKind::Gradle,
            &fingerprint,
            module_key,
            |m| {
                m.java_compile_config = Some(config.clone());
                // Keep populating the legacy classpath field for older readers.
                m.classpath = Some(config.compile_classpath.clone());
            },
        )?;

        // Best-effort: persist a workspace-local Gradle model snapshot so `nova-project`
        // can reuse resolved source roots/classpaths without invoking Gradle during discovery.
        let _ = update_gradle_snapshot_java_compile_config(
            project_root,
            &fingerprint,
            &project_path_for_snapshot,
            &project_dir,
            &config,
        );

        Ok(config)
    }

    pub fn build(
        &self,
        project_root: &Path,
        project_path: Option<&str>,
        cache: &BuildCache,
    ) -> Result<BuildResult> {
        self.build_with_task(
            project_root,
            project_path,
            GradleBuildTask::CompileJava,
            cache,
        )
    }

    pub fn build_with_task(
        &self,
        project_root: &Path,
        project_path: Option<&str>,
        task: GradleBuildTask,
        cache: &BuildCache,
    ) -> Result<BuildResult> {
        let project_path = project_path.filter(|p| *p != ":");
        let fingerprint = gradle_build_fingerprint(project_root)?;
        let module_key = project_path.unwrap_or("<root>");

        let (program, args, output) = self.run_compile(project_root, project_path, task)?;
        let command = format_command(&program, &args);
        let combined = output.combined();
        let diagnostics = crate::parse_javac_diagnostics(&combined, "gradle");

        cache.update_module(
            project_root,
            BuildSystemKind::Gradle,
            &fingerprint,
            module_key,
            |m| {
                m.diagnostics = Some(
                    diagnostics
                        .iter()
                        .map(crate::cache::CachedDiagnostic::from)
                        .collect(),
                );
            },
        )?;

        if output.status.success() || !diagnostics.is_empty() {
            return Ok(BuildResult {
                diagnostics,
                tool: Some("gradle".to_string()),
                command: Some(command),
                exit_code: output.status.code(),
                stdout: output.stdout,
                stderr: output.stderr,
                output_truncated: output.truncated,
            });
        }

        Err(BuildError::CommandFailed {
            tool: "gradle",
            command,
            code: output.status.code(),
            stdout: output.stdout,
            stderr: output.stderr,
            output_truncated: output.truncated,
        })
    }

    pub fn annotation_processing(
        &self,
        project_root: &Path,
        project_path: Option<&str>,
        cache: &BuildCache,
    ) -> Result<AnnotationProcessing> {
        let project_path = project_path.filter(|p| *p != ":");
        let fingerprint = gradle_build_fingerprint(project_root)?;
        let module_key = project_path.unwrap_or("<root>");

        if let Some(cached) = cache.get_module(
            project_root,
            BuildSystemKind::Gradle,
            &fingerprint,
            module_key,
        )? {
            if let Some(cfg) = cached.annotation_processing {
                return Ok(cfg);
            }
        }

        let (program, args, output) =
            self.run_print_annotation_processing(project_root, project_path)?;
        if !output.status.success() {
            return Err(BuildError::CommandFailed {
                tool: "gradle",
                command: format_command(&program, &args),
                code: output.status.code(),
                stdout: output.stdout,
                stderr: output.stderr,
                output_truncated: output.truncated,
            });
        }

        let combined = output.combined();
        let parsed = parse_gradle_annotation_processing_json(&combined)?;
        let mut config = AnnotationProcessing {
            main: parsed.main.map(annotation_processing_from_gradle),
            test: parsed.test.map(annotation_processing_from_gradle),
        };

        // Fill in conventional defaults when the Gradle model doesn't expose a value.
        let needs_fallback = config
            .main
            .as_ref()
            .is_some_and(|m| m.generated_sources_dir.is_none())
            || config
                .test
                .as_ref()
                .is_some_and(|t| t.generated_sources_dir.is_none());

        let project_dir = if needs_fallback {
            match parsed
                .project_dir
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                Some(dir) => PathBuf::from(dir),
                None => gradle_project_dir_cached(project_root, project_path, cache, &fingerprint)?,
            }
        } else {
            // Dummy value; should not be used when `needs_fallback` is false.
            PathBuf::new()
        };
        if let Some(main) = config.main.as_mut() {
            if main.generated_sources_dir.is_none() {
                main.generated_sources_dir =
                    Some(project_dir.join("build/generated/sources/annotationProcessor/java/main"));
            }
        }
        if let Some(test) = config.test.as_mut() {
            if test.generated_sources_dir.is_none() {
                test.generated_sources_dir =
                    Some(project_dir.join("build/generated/sources/annotationProcessor/java/test"));
            }
        }

        cache.update_module(
            project_root,
            BuildSystemKind::Gradle,
            &fingerprint,
            module_key,
            |m| {
                m.annotation_processing = Some(config.clone());
            },
        )?;

        Ok(config)
    }

    fn run_print_java_compile_config(
        &self,
        project_root: &Path,
        project_path: Option<&str>,
    ) -> Result<(PathBuf, Vec<String>, CommandOutput)> {
        let (program, mut args) = self.gradle_program_and_prefix_args(project_root);
        let init_script = write_init_script(project_root)?;
        let buildsrc_suffix = project_path.and_then(gradle_buildsrc_suffix);
        let is_buildsrc = buildsrc_suffix.is_some();
        let buildsrc_nested_project_path =
            buildsrc_suffix.and_then(|rest| if rest.is_empty() { None } else { Some(rest) });

        args.push("--no-daemon".into());
        args.push("--console=plain".into());
        args.push("-q".into());
        args.push("--init-script".into());
        args.push(init_script.to_string_lossy().to_string());
        if is_buildsrc {
            args.push("--project-dir".into());
            args.push(GRADLE_BUILDSRC_PROJECT_DIR.to_string());
        }

        let task = if let Some(nested_path) = buildsrc_nested_project_path {
            format!("{nested_path}:{NOVA_GRADLE_TASK}")
        } else if is_buildsrc {
            NOVA_GRADLE_TASK.to_string()
        } else {
            match project_path {
                Some(p) => format!("{p}:{NOVA_GRADLE_TASK}"),
                None => NOVA_GRADLE_TASK.to_string(),
            }
        };
        args.push(task);

        let output = self.runner.run(project_root, &program, &args);
        let _ = std::fs::remove_file(&init_script);
        Ok((program, args, output?))
    }

    fn run_print_annotation_processing(
        &self,
        project_root: &Path,
        project_path: Option<&str>,
    ) -> Result<(PathBuf, Vec<String>, CommandOutput)> {
        let (program, mut args) = self.gradle_program_and_prefix_args(project_root);
        let init_script = write_init_script(project_root)?;
        let buildsrc_suffix = project_path.and_then(gradle_buildsrc_suffix);
        let is_buildsrc = buildsrc_suffix.is_some();
        let buildsrc_nested_project_path =
            buildsrc_suffix.and_then(|rest| if rest.is_empty() { None } else { Some(rest) });

        args.push("--no-daemon".into());
        args.push("--console=plain".into());
        args.push("-q".into());
        args.push("--init-script".into());
        args.push(init_script.to_string_lossy().to_string());
        if is_buildsrc {
            args.push("--project-dir".into());
            args.push(GRADLE_BUILDSRC_PROJECT_DIR.to_string());
        }

        let task = if let Some(nested_path) = buildsrc_nested_project_path {
            format!("{nested_path}:{NOVA_GRADLE_APT_TASK}")
        } else if is_buildsrc {
            NOVA_GRADLE_APT_TASK.to_string()
        } else {
            match project_path {
                Some(p) => format!("{p}:{NOVA_GRADLE_APT_TASK}"),
                None => NOVA_GRADLE_APT_TASK.to_string(),
            }
        };
        args.push(task);

        let output = self.runner.run(project_root, &program, &args);
        let _ = std::fs::remove_file(&init_script);
        Ok((program, args, output?))
    }

    fn run_print_projects(
        &self,
        project_root: &Path,
    ) -> Result<(PathBuf, Vec<String>, CommandOutput)> {
        let (program, mut args) = self.gradle_program_and_prefix_args(project_root);
        let init_script = write_init_script(project_root)?;

        args.push("--no-daemon".into());
        args.push("--console=plain".into());
        args.push("-q".into());
        args.push("--init-script".into());
        args.push(init_script.to_string_lossy().to_string());
        args.push("printNovaProjects".into());

        let output = self.runner.run(project_root, &program, &args);
        let _ = std::fs::remove_file(&init_script);
        Ok((program, args, output?))
    }

    fn run_print_all_java_compile_configs(
        &self,
        project_root: &Path,
    ) -> Result<(PathBuf, Vec<String>, CommandOutput)> {
        let (program, mut args) = self.gradle_program_and_prefix_args(project_root);
        let init_script = write_init_script(project_root)?;

        args.push("--no-daemon".into());
        args.push("--console=plain".into());
        args.push("-q".into());
        args.push("--init-script".into());
        args.push(init_script.to_string_lossy().to_string());
        args.push(NOVA_GRADLE_ALL_TASK.to_string());

        let output = self.runner.run(project_root, &program, &args);
        let _ = std::fs::remove_file(&init_script);
        Ok((program, args, output?))
    }

    fn run_compile(
        &self,
        project_root: &Path,
        project_path: Option<&str>,
        task: GradleBuildTask,
    ) -> Result<(PathBuf, Vec<String>, CommandOutput)> {
        let (program, mut args) = self.gradle_program_and_prefix_args(project_root);
        args.push("--no-daemon".into());
        args.push("--console=plain".into());
        let buildsrc_suffix = project_path.and_then(gradle_buildsrc_suffix);
        let is_buildsrc = buildsrc_suffix.is_some();
        let buildsrc_nested_project_path =
            buildsrc_suffix.and_then(|rest| if rest.is_empty() { None } else { Some(rest) });
        if is_buildsrc {
            args.push("--project-dir".into());
            args.push(GRADLE_BUILDSRC_PROJECT_DIR.to_string());
        }

        let task_name = match task {
            GradleBuildTask::CompileJava => "compileJava",
            GradleBuildTask::CompileTestJava => "compileTestJava",
        };

        if is_buildsrc {
            let task_arg = match buildsrc_nested_project_path {
                Some(nested_path) => format!("{nested_path}:{task_name}"),
                None => task_name.to_string(),
            };
            args.push(task_arg);
            let output = self.runner.run(project_root, &program, &args)?;
            return Ok((program, args, output));
        }

        match project_path {
            Some(p) => {
                args.push(format!("{p}:{task_name}"));
                let output = self.runner.run(project_root, &program, &args)?;
                Ok((program, args, output))
            }
            None => {
                let (init_script, root_task) = match task {
                    GradleBuildTask::CompileJava => (
                        write_compile_all_java_init_script(project_root)?,
                        "novaCompileAllJava",
                    ),
                    GradleBuildTask::CompileTestJava => (
                        write_compile_all_test_java_init_script(project_root)?,
                        "novaCompileAllTestJava",
                    ),
                };
                args.push("--init-script".into());
                args.push(init_script.to_string_lossy().to_string());
                args.push(root_task.to_string());

                let output = self.runner.run(project_root, &program, &args);
                let _ = std::fs::remove_file(&init_script);
                Ok((program, args, output?))
            }
        }
    }

    fn gradle_program_and_prefix_args(&self, project_root: &Path) -> (PathBuf, Vec<String>) {
        if self.config.prefer_wrapper {
            #[cfg(windows)]
            {
                let wrapper = project_root.join("gradlew.bat");
                if wrapper.exists() {
                    return (wrapper, Vec::new());
                }
            }

            #[cfg(not(windows))]
            {
                let wrapper = project_root.join("gradlew");
                if wrapper.exists() {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;

                        // Many projects check `gradlew` into source control without the executable
                        // bit set (common for Windows-originated checkouts). Instead of failing with
                        // PermissionDenied when invoking the wrapper, fall back to `sh gradlew`.
                        if std::fs::metadata(&wrapper)
                            .map(|m| m.permissions().mode() & 0o111 != 0)
                            .unwrap_or(false)
                        {
                            return (wrapper, Vec::new());
                        }

                        return (
                            PathBuf::from("sh"),
                            vec![wrapper.to_string_lossy().to_string()],
                        );
                    }

                    #[cfg(not(unix))]
                    {
                        return (wrapper, Vec::new());
                    }
                }
            }
        }
        (self.config.gradle_path.clone(), Vec::new())
    }
}

fn gradle_build_fingerprint(project_root: &Path) -> Result<BuildFileFingerprint> {
    let build_files = collect_gradle_build_files(project_root)?;
    Ok(BuildFileFingerprint::from_files(project_root, build_files)?)
}

fn is_word_byte(b: u8) -> bool {
    // Keep semantics aligned with Regex `\b` for ASCII: alphanumeric + underscore.
    b.is_ascii_alphanumeric() || b == b'_'
}

fn matches_keyword_at(bytes: &[u8], start: usize, keyword: &[u8]) -> bool {
    if keyword.is_empty() || start >= bytes.len() || !bytes[start..].starts_with(keyword) {
        return false;
    }

    let prev_is_word = start
        .checked_sub(1)
        .and_then(|idx| bytes.get(idx))
        .is_some_and(|b| is_word_byte(*b));
    let next_is_word = bytes
        .get(start + keyword.len())
        .is_some_and(|b| is_word_byte(*b));
    !prev_is_word && !next_is_word
}

fn gradle_settings_contains_includes(contents: &str) -> bool {
    // Best-effort parser: scan the settings file ignoring comments and string literals, and look
    // for Gradle settings directives that indicate a multi-project build:
    // - `include`
    // - `includeFlat`
    // - `includeBuild`
    //
    // This avoids false positives such as `rootProject.name = "includeFlat-root"`.
    let bytes = contents.as_bytes();
    let mut i = 0usize;

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

        // Outside strings/comments.
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

        if matches_keyword_at(bytes, i, b"includeFlat")
            || matches_keyword_at(bytes, i, b"includeBuild")
            || matches_keyword_at(bytes, i, b"include")
        {
            return true;
        }

        i += 1;
    }

    false
}

fn gradle_settings_suggest_multi_project(project_root: &Path) -> bool {
    // Best-effort heuristic: only attempt the batch `printNovaAllJavaCompileConfigs` task when the
    // build looks multi-project. Avoids doing extra work for single-project builds where callers
    // ask for a specific `:app`-style project path even though the workspace has no `include`.
    //
    // We intentionally keep this heuristic simple and fast: check for a `settings.gradle*` file
    // that contains the substring `include` (covers `include`, `includeBuild`, `includeFlat`).
    for name in ["settings.gradle.kts", "settings.gradle"] {
        let path = project_root.join(name);
        if !path.is_file() {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        if gradle_settings_contains_includes(&contents) {
            return true;
        }
    }
    false
}

fn gradle_output_dir_cached(
    project_root: &Path,
    project_path: Option<&str>,
    cache: &BuildCache,
    fingerprint: &BuildFileFingerprint,
) -> Result<PathBuf> {
    let Some(project_path) = project_path else {
        return Ok(gradle_output_dir(project_root, None));
    };

    // Root project path can't be used as a task prefix (it would produce
    // `::printNovaJavaCompileConfig`), so callers use `None` instead.
    if project_path == ":" {
        return Ok(gradle_output_dir(project_root, None));
    }

    if let Some(data) = cache.load(project_root, BuildSystemKind::Gradle, fingerprint)? {
        if let Some(projects) = data.projects {
            if let Some(found) = projects.into_iter().find(|p| p.path == project_path) {
                return Ok(found
                    .dir
                    .join("build")
                    .join("classes")
                    .join("java")
                    .join("main"));
            }
        }
    }

    Ok(gradle_output_dir(project_root, Some(project_path)))
}

fn gradle_project_dir_cached(
    project_root: &Path,
    project_path: Option<&str>,
    cache: &BuildCache,
    fingerprint: &BuildFileFingerprint,
) -> Result<PathBuf> {
    let Some(project_path) = project_path else {
        return Ok(project_root.to_path_buf());
    };

    if project_path == ":" {
        return Ok(project_root.to_path_buf());
    }
    if let Some(rest) = gradle_buildsrc_suffix(project_path) {
        let mut rel = PathBuf::from(GRADLE_BUILDSRC_PROJECT_DIR);
        let trimmed = rest.trim_matches(':');
        for part in trimmed.split(':').filter(|p| !p.is_empty()) {
            rel.push(part);
        }
        return Ok(project_root.join(rel));
    }

    if let Some(data) = cache.load(project_root, BuildSystemKind::Gradle, fingerprint)? {
        if let Some(projects) = data.projects {
            if let Some(found) = projects.into_iter().find(|p| p.path == project_path) {
                return Ok(found.dir);
            }
        }
    }

    // Heuristic mapping: `:lib:core` -> `<root>/lib/core`.
    let mut rel = PathBuf::new();
    let trimmed = project_path.trim_matches(':');
    for part in trimmed.split(':').filter(|p| !p.is_empty()) {
        rel.push(part);
    }
    Ok(project_root.join(rel))
}

fn gradle_output_dir(project_root: &Path, project_path: Option<&str>) -> PathBuf {
    // Best-effort mapping from Gradle project paths to directories.
    //
    // For standard Gradle layouts, a project path like `:app` corresponds to an
    // `app/` directory under the workspace root. More complex setups can change
    // this mapping using `settings.gradle`, but we keep the heuristic small and
    // predictable.
    let mut rel = PathBuf::new();
    if let Some(path) = project_path {
        if let Some(rest) = gradle_buildsrc_suffix(path) {
            rel.push(GRADLE_BUILDSRC_PROJECT_DIR);
            let trimmed = rest.trim_matches(':');
            for part in trimmed.split(':').filter(|p| !p.is_empty()) {
                rel.push(part);
            }
        } else {
            let trimmed = path.trim_matches(':');
            for part in trimmed.split(':').filter(|p| !p.is_empty()) {
                rel.push(part);
            }
        }
    }

    project_root
        .join(rel)
        .join("build")
        .join("classes")
        .join("java")
        .join("main")
}

fn gradle_test_output_dir_from_main(main_output_dir: &Path) -> PathBuf {
    let mut path = main_output_dir.to_path_buf();
    path.pop();
    path.push("test");
    path
}

fn collect_source_roots(project_dir: &Path, source_set: &str) -> Vec<PathBuf> {
    // Best-effort fallback when Gradle's `sourceSets` extension isn't available
    // (e.g. aggregator roots without the Java plugin applied).
    let dir = project_dir.join("src").join(source_set).join("java");
    if dir.is_dir() {
        vec![dir]
    } else {
        Vec::new()
    }
}

fn discover_conventional_gradle_source_roots(project_dir: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let src_dir = project_dir.join("src");
    if !src_dir.is_dir() {
        return (Vec::new(), Vec::new());
    }

    let mut main_roots = Vec::new();
    let mut test_roots = Vec::new();

    let Ok(entries) = std::fs::read_dir(&src_dir) else {
        return (main_roots, test_roots);
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        // We only care about the conventional `src/<sourceSet>/java` layout.
        let java_dir = path.join("java");
        if !java_dir.is_dir() {
            continue;
        }

        let source_set_name = entry.file_name().to_string_lossy().to_string();
        if source_set_name.to_ascii_lowercase().contains("test") {
            test_roots.push(java_dir);
        } else {
            main_roots.push(java_dir);
        }
    }

    // Stabilize ordering across filesystems.
    main_roots.sort();
    test_roots.sort();

    (main_roots, test_roots)
}

fn augment_java_compile_config_with_conventional_gradle_source_roots(
    project_dir: &Path,
    config: &mut JavaCompileConfig,
) {
    let (mut main_roots, mut test_roots) = discover_conventional_gradle_source_roots(project_dir);

    // Preserve existing ordering by appending the newly discovered entries.
    config.main_source_roots.append(&mut main_roots);
    config.test_source_roots.append(&mut test_roots);

    dedupe_paths(&mut config.main_source_roots);
    dedupe_paths(&mut config.test_source_roots);

    // Backwards compat: if we still have no roots, fall back to the historical main/test defaults.
    if config.main_source_roots.is_empty() {
        config.main_source_roots = collect_source_roots(project_dir, "main");
    }
    if config.test_source_roots.is_empty() {
        config.test_source_roots = collect_source_roots(project_dir, "test");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::ExitStatus;
    use std::sync::Mutex;
    use tempfile::tempdir;

    #[test]
    fn gradle_settings_suggest_multi_project_ignores_includes_in_strings_and_comments() {
        let tmp = tempdir().unwrap();
        let project_root = tmp.path();

        std::fs::write(
            project_root.join("settings.gradle"),
            r#"
rootProject.name = "includeFlat-root"

// include(":app")
/* includeFlat(":lib") */
val doc = """includeBuild(":other")"""
"#,
        )
        .unwrap();

        assert!(!gradle_settings_suggest_multi_project(project_root));
    }

    #[test]
    fn gradle_settings_suggest_multi_project_detects_include_directives() {
        let tmp = tempdir().unwrap();
        let project_root = tmp.path();

        std::fs::write(
            project_root.join("settings.gradle.kts"),
            "include(\":app\")\n",
        )
        .unwrap();

        assert!(gradle_settings_suggest_multi_project(project_root));
    }

    #[test]
    fn gradle_output_dir_maps_project_path_to_directory() {
        let root = Path::new("/workspace");
        assert_eq!(
            gradle_output_dir(root, None),
            PathBuf::from("/workspace/build/classes/java/main")
        );
        assert_eq!(
            gradle_output_dir(root, Some(":app")),
            PathBuf::from("/workspace/app/build/classes/java/main")
        );
        assert_eq!(
            gradle_output_dir(root, Some(GRADLE_BUILDSRC_PROJECT_PATH)),
            PathBuf::from("/workspace/buildSrc/build/classes/java/main")
        );
        assert_eq!(
            gradle_output_dir(root, Some(":__buildSrc:plugins")),
            PathBuf::from("/workspace/buildSrc/plugins/build/classes/java/main")
        );
        assert_eq!(
            gradle_output_dir(root, Some(":lib:core")),
            PathBuf::from("/workspace/lib/core/build/classes/java/main")
        );
    }

    #[test]
    fn parses_gradle_projects_json_from_noisy_output() {
        let out = r#"
> Task :printNovaProjects
Deprecated feature warning
NOVA_PROJECTS_BEGIN
{"projects":[{"path":":app","projectDir":"/workspace/app"},{"path":":","projectDir":"/workspace"}]}
NOVA_PROJECTS_END
BUILD SUCCESSFUL
"#;
        let projects = parse_gradle_projects_output(out).unwrap();
        assert_eq!(
            projects,
            vec![
                GradleProjectInfo {
                    path: ":".into(),
                    dir: PathBuf::from("/workspace"),
                },
                GradleProjectInfo {
                    path: ":app".into(),
                    dir: PathBuf::from("/workspace/app"),
                }
            ]
        );
    }

    #[test]
    fn union_classpath_preserves_order_and_dedupes() {
        let union = JavaCompileConfig::union([
            JavaCompileConfig {
                compile_classpath: vec![
                    PathBuf::from("/a"),
                    PathBuf::from("/b"),
                    PathBuf::from("/c"),
                ],
                ..JavaCompileConfig::default()
            },
            JavaCompileConfig {
                compile_classpath: vec![PathBuf::from("/b"), PathBuf::from("/d")],
                ..JavaCompileConfig::default()
            },
            JavaCompileConfig {
                compile_classpath: vec![PathBuf::from("/a"), PathBuf::from("/e")],
                ..JavaCompileConfig::default()
            },
        ]);
        assert_eq!(
            union.compile_classpath,
            vec![
                PathBuf::from("/a"),
                PathBuf::from("/b"),
                PathBuf::from("/c"),
                PathBuf::from("/d"),
                PathBuf::from("/e")
            ]
        );
    }

    #[test]
    fn parse_gradle_classpath_ignores_nova_markers() {
        let out = "NOVA_JSON_BEGIN\n/a/b/c.jar\n";
        let cp = parse_gradle_classpath_output(out);
        assert_eq!(cp, vec![PathBuf::from("/a/b/c.jar")]);
    }

    #[test]
    fn extracts_nova_json_block_from_gradle_noise() {
        let out = r#"
> Task :someTask

NOVA_JSON_BEGIN
{"compileClasspath":["/a.jar"]}
NOVA_JSON_END

BUILD SUCCESSFUL in 1s
"#;
        let json = extract_nova_json_block(out).unwrap();
        assert_eq!(json.trim(), r#"{"compileClasspath":["/a.jar"]}"#);
    }

    #[test]
    fn parses_gradle_java_compile_config_and_dedupes_paths() {
        let out = r#"
NOVA_JSON_BEGIN
{
  "compileClasspath": ["/dep/a.jar", "/dep/a.jar", "/dep/b.jar"],
  "testCompileClasspath": ["/dep/test.jar"],
  "mainSourceRoots": ["/src/main/java"],
  "testSourceRoots": ["/src/test/java"],
  "mainOutputDirs": ["/out/main", "/out/main"],
  "testOutputDirs": ["/out/test"],
  "sourceCompatibility": "17",
  "targetCompatibility": "17",
  "toolchainLanguageVersion": "21"
}
NOVA_JSON_END
"#;
        let parsed = parse_gradle_java_compile_config_json(out).expect("parse json");
        let cfg = normalize_gradle_java_compile_config(
            parsed,
            PathBuf::from("/fallback/main"),
            PathBuf::from("/fallback/test"),
        );
        assert_eq!(cfg.main_source_roots, vec![PathBuf::from("/src/main/java")]);
        assert_eq!(cfg.test_source_roots, vec![PathBuf::from("/src/test/java")]);
        assert_eq!(cfg.main_output_dir, Some(PathBuf::from("/out/main")));
        assert_eq!(cfg.test_output_dir, Some(PathBuf::from("/out/test")));
        assert_eq!(
            cfg.compile_classpath,
            vec![
                PathBuf::from("/out/main"),
                PathBuf::from("/dep/a.jar"),
                PathBuf::from("/dep/b.jar")
            ]
        );
        assert_eq!(
            cfg.test_classpath,
            vec![
                PathBuf::from("/out/test"),
                PathBuf::from("/out/main"),
                PathBuf::from("/dep/test.jar")
            ]
        );
        assert_eq!(cfg.source.as_deref(), Some("17"));
        assert_eq!(cfg.target.as_deref(), Some("17"));
        assert_eq!(cfg.release.as_deref(), Some("21"));
    }

    #[test]
    fn parses_gradle_java_compile_config_with_null_fields() {
        let out = r#"
some warning
NOVA_JSON_BEGIN
{"compileClasspath":null,"testCompileClasspath":null,"mainOutputDirs":null,"testOutputDirs":null}
NOVA_JSON_END
"#;
        let parsed = parse_gradle_java_compile_config_json(out).expect("parse json");
        let main_output_fallback = gradle_output_dir(Path::new("/workspace"), Some(":app"));
        let test_output_fallback = gradle_test_output_dir_from_main(&main_output_fallback);
        let cfg = normalize_gradle_java_compile_config(
            parsed,
            main_output_fallback,
            test_output_fallback,
        );
        assert_eq!(
            cfg.main_output_dir,
            Some(PathBuf::from("/workspace/app/build/classes/java/main"))
        );
        assert_eq!(
            cfg.test_output_dir,
            Some(PathBuf::from("/workspace/app/build/classes/java/test"))
        );
        assert_eq!(
            cfg.compile_classpath,
            vec![PathBuf::from("/workspace/app/build/classes/java/main")]
        );
        assert_eq!(
            cfg.test_classpath,
            vec![
                PathBuf::from("/workspace/app/build/classes/java/test"),
                PathBuf::from("/workspace/app/build/classes/java/main")
            ]
        );
    }

    #[test]
    fn gradle_java_compile_config_infers_module_path_and_enable_preview() {
        let testdata_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../nova-classpath/testdata");
        let named = testdata_dir.join("named-module.jar");
        let automatic = testdata_dir.join("automatic-module-name-1.2.3.jar");
        let dep = testdata_dir.join("dep.jar");

        let payload = serde_json::json!({
            "compileClasspath": [
                named.to_string_lossy(),
                automatic.to_string_lossy(),
                dep.to_string_lossy(),
            ],
            "testCompileClasspath": [],
            "mainSourceRoots": [],
            "testSourceRoots": [],
            "mainOutputDirs": ["/out/main"],
            "testOutputDirs": ["/out/test"],
            "compileCompilerArgs": ["--enable-preview"],
            "testCompilerArgs": [],
            "inferModulePath": true,
        });

        let out = format!(
            "NOVA_JSON_BEGIN\n{}\nNOVA_JSON_END\n",
            serde_json::to_string(&payload).unwrap()
        );

        let parsed = parse_gradle_java_compile_config_json(&out).expect("parse json");
        let cfg = normalize_gradle_java_compile_config(
            parsed,
            PathBuf::from("/fallback/main"),
            PathBuf::from("/fallback/test"),
        );

        assert!(cfg.enable_preview);
        assert_eq!(cfg.module_path, vec![named, automatic]);
    }

    #[derive(Debug)]
    struct StaticGradleRunner {
        invocations: Mutex<Vec<Vec<String>>>,
        output: CommandOutput,
    }

    impl StaticGradleRunner {
        fn new(output: CommandOutput) -> Self {
            Self {
                invocations: Mutex::new(Vec::new()),
                output,
            }
        }

        fn invocations(&self) -> Vec<Vec<String>> {
            self.invocations.lock().expect("lock poisoned").clone()
        }
    }

    impl CommandRunner for StaticGradleRunner {
        fn run(
            &self,
            _cwd: &Path,
            _program: &Path,
            args: &[String],
        ) -> std::io::Result<CommandOutput> {
            self.invocations
                .lock()
                .expect("lock poisoned")
                .push(args.to_vec());
            Ok(self.output.clone())
        }
    }

    fn exit_status(code: i32) -> ExitStatus {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            ExitStatus::from_raw(code << 8)
        }

        #[cfg(windows)]
        {
            use std::os::windows::process::ExitStatusExt;
            ExitStatus::from_raw(code as u32)
        }
    }

    fn output(code: i32, stdout: &str, stderr: &str) -> CommandOutput {
        CommandOutput {
            status: exit_status(code),
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
            truncated: false,
        }
    }

    #[test]
    fn java_compile_config_uses_project_dir_from_payload_for_fallbacks() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("workspace");
        std::fs::create_dir_all(&project_root).unwrap();
        std::fs::write(project_root.join("settings.gradle"), "").unwrap();

        // Simulate a custom Gradle projectDir mapping where `:app` does *not*
        // correspond to `<root>/app`.
        let app_dir = project_root.join("custom").join("app");
        std::fs::create_dir_all(app_dir.join("src/main/java")).unwrap();
        std::fs::create_dir_all(app_dir.join("src/test/java")).unwrap();

        let payload = serde_json::json!({
            "projectPath": ":app",
            "projectDir": app_dir.to_string_lossy().to_string(),
            "compileClasspath": [],
            "testCompileClasspath": [],
            "mainSourceRoots": [],
            "testSourceRoots": [],
            "mainOutputDirs": null,
            "testOutputDirs": null,
        });
        let stdout = format!(
            "NOVA_JSON_BEGIN\n{}\nNOVA_JSON_END\n",
            serde_json::to_string(&payload).unwrap()
        );

        let runner = Arc::new(StaticGradleRunner::new(output(0, &stdout, "")));
        let gradle = GradleBuild::with_runner(GradleConfig::default(), runner.clone());
        let cache = BuildCache::new(tmp.path().join("cache"));

        let cfg = gradle
            .java_compile_config(&project_root, Some(":app"), &cache)
            .unwrap();

        assert_eq!(
            cfg.main_output_dir,
            Some(app_dir.join("build/classes/java/main"))
        );
        assert_eq!(
            cfg.test_output_dir,
            Some(app_dir.join("build/classes/java/test"))
        );
        assert_eq!(cfg.main_source_roots, vec![app_dir.join("src/main/java")]);
        assert_eq!(cfg.test_source_roots, vec![app_dir.join("src/test/java")]);

        // Sanity check: we invoked the project-scoped task.
        let invocations = runner.invocations();
        assert_eq!(invocations.len(), 1);
        assert!(invocations[0]
            .iter()
            .any(|arg| arg == ":app:printNovaJavaCompileConfig"));
    }

    #[test]
    fn augments_gradle_java_compile_config_with_custom_source_sets() {
        let dir = tempdir().expect("tempdir");
        let project_dir = dir.path();

        std::fs::create_dir_all(project_dir.join("src/main/java")).expect("create main");
        std::fs::create_dir_all(project_dir.join("src/test/java")).expect("create test");
        std::fs::create_dir_all(project_dir.join("src/integrationTest/java"))
            .expect("create integrationTest");

        let mut cfg = JavaCompileConfig {
            main_source_roots: vec![project_dir.join("src/main/java")],
            test_source_roots: vec![project_dir.join("src/test/java")],
            ..JavaCompileConfig::default()
        };

        augment_java_compile_config_with_conventional_gradle_source_roots(project_dir, &mut cfg);

        assert_eq!(
            cfg.main_source_roots,
            vec![project_dir.join("src/main/java")]
        );
        assert_eq!(
            cfg.test_source_roots,
            vec![
                project_dir.join("src/test/java"),
                project_dir.join("src/integrationTest/java")
            ]
        );
    }
}

pub fn parse_gradle_classpath_output(output: &str) -> Vec<PathBuf> {
    let mut entries = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with("NOVA_") {
            continue;
        }
        if line.starts_with('>') || line.starts_with("FAILURE") || line.starts_with("BUILD FAILED")
        {
            continue;
        }
        // If a tool prints a single platform-separated classpath string, fall
        // back to `split_paths`.
        let split: Vec<_> = std::env::split_paths(line).collect();
        if split.len() > 1 {
            entries.extend(split);
        } else {
            entries.push(PathBuf::from(line));
        }
    }
    let mut seen = std::collections::HashSet::new();
    entries.retain(|p| seen.insert(p.clone()));
    entries
}

pub fn parse_gradle_projects_output(output: &str) -> Result<Vec<GradleProjectInfo>> {
    let json = extract_sentinel_block(output, NOVA_PROJECTS_BEGIN, NOVA_PROJECTS_END)
        .ok_or_else(|| BuildError::Parse("failed to locate Gradle project JSON block".into()))?;

    let parsed: GradleProjectsJson =
        serde_json::from_str(json.trim()).map_err(|e| BuildError::Parse(e.to_string()))?;

    let mut projects: Vec<GradleProjectInfo> = parsed
        .projects
        .into_iter()
        .map(|p| GradleProjectInfo {
            path: p.path,
            dir: PathBuf::from(p.project_dir),
        })
        .collect();
    projects.sort_by(|a, b| a.path.cmp(&b.path));
    projects.dedup_by(|a, b| a.path == b.path);
    Ok(projects)
}

fn parse_gradle_all_java_compile_configs_output(
    output: &str,
) -> Result<GradleAllJavaCompileConfigsJson> {
    let json = extract_sentinel_block(output, NOVA_ALL_JSON_BEGIN, NOVA_ALL_JSON_END).ok_or_else(
        || {
            BuildError::Parse(
                "failed to locate Gradle all-project Java compile config JSON block".into(),
            )
        },
    )?;

    serde_json::from_str(json.trim()).map_err(|e| BuildError::Parse(e.to_string()))
}

fn parse_gradle_annotation_processing_json(output: &str) -> Result<GradleAnnotationProcessingJson> {
    let json = extract_sentinel_block(output, NOVA_APT_BEGIN, NOVA_APT_END)
        .or_else(|| extract_json_payload(output).map(str::to_string))
        .ok_or_else(|| {
            BuildError::Parse("failed to locate Gradle annotation processing JSON".into())
        })?;

    serde_json::from_str(json.trim()).map_err(|e| BuildError::Parse(e.to_string()))
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GradleAnnotationProcessingJson {
    #[serde(default)]
    project_dir: Option<String>,
    #[serde(default)]
    main: Option<GradleJavaCompileAptJson>,
    #[serde(default)]
    test: Option<GradleJavaCompileAptJson>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GradleJavaCompileAptJson {
    #[serde(default)]
    annotation_processor_path: Vec<String>,
    #[serde(default)]
    compiler_args: Vec<String>,
    #[serde(default)]
    generated_sources_dir: Option<String>,
}

pub fn parse_gradle_annotation_processing_output(output: &str) -> Result<AnnotationProcessing> {
    let parsed = parse_gradle_annotation_processing_json(output)?;

    Ok(AnnotationProcessing {
        main: parsed.main.map(annotation_processing_from_gradle),
        test: parsed.test.map(annotation_processing_from_gradle),
    })
}

fn extract_json_payload(output: &str) -> Option<&str> {
    let start = output.find('{')?;
    let end = output.rfind('}')?;
    if end < start {
        return None;
    }
    Some(&output[start..=end])
}

fn annotation_processing_from_gradle(json: GradleJavaCompileAptJson) -> AnnotationProcessingConfig {
    let GradleJavaCompileAptJson {
        annotation_processor_path,
        compiler_args,
        generated_sources_dir,
    } = json;

    let mut config = AnnotationProcessingConfig {
        processor_path: annotation_processor_path
            .into_iter()
            .map(PathBuf::from)
            .collect(),
        compiler_args: compiler_args.clone(),
        generated_sources_dir: generated_sources_dir.map(PathBuf::from),
        ..AnnotationProcessingConfig::default()
    };

    let mut proc_mode = None::<String>;
    let mut compiler_args = compiler_args.into_iter().peekable();
    while let Some(arg) = compiler_args.next() {
        match arg.as_str() {
            "-processor" => {
                if let Some(value) = compiler_args.next() {
                    config.processors.extend(
                        value
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty()),
                    );
                }
            }
            "-processorpath" | "--processor-path" => {
                if let Some(value) = compiler_args.next() {
                    config.processor_path.extend(split_path_list(&value));
                }
            }
            "-s" => {
                if let Some(value) = compiler_args.next() {
                    config.generated_sources_dir = Some(PathBuf::from(value));
                }
            }
            other if other.starts_with("-proc:") => {
                proc_mode = Some(other.trim_start_matches("-proc:").to_string());
            }
            other if other.starts_with("-A") => {
                let rest = other.trim_start_matches("-A");
                let (k, v) = rest.split_once('=').unwrap_or((rest, ""));
                if !k.is_empty() {
                    config.options.insert(k.to_string(), v.to_string());
                }
            }
            _ => {}
        }
    }

    config.enabled = match proc_mode.as_deref() {
        Some("none") => false,
        Some(_) => true,
        None => true,
    };

    let mut seen_processors = std::collections::HashSet::new();
    config
        .processors
        .retain(|p| seen_processors.insert(p.clone()));

    let mut seen_paths = std::collections::HashSet::new();
    config
        .processor_path
        .retain(|p| seen_paths.insert(p.clone()));

    config
}

fn split_path_list(value: &str) -> Vec<PathBuf> {
    if value.is_empty() {
        return Vec::new();
    }

    // Prefer `;` if it appears anywhere in the argument. This matches the platform default on
    // Windows and avoids breaking `C:\...` drive letters when we only see a single entry.
    if value.contains(';') {
        return value
            .split(';')
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect();
    }

    // On Unix, `javac` uses `:` to separate path entries. Avoid splitting Windows drive letters.
    let bytes = value.as_bytes();
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b':' {
            let is_drive_letter = i > start
                && i - start == 1
                && bytes[i - 1].is_ascii_alphabetic()
                && matches!(bytes.get(i + 1).copied(), Some(b'\\') | Some(b'/'));
            if !is_drive_letter {
                let part = &value[start..i];
                if !part.is_empty() {
                    parts.push(PathBuf::from(part));
                }
                start = i + 1;
            }
        }
        i += 1;
    }
    let tail = &value[start..];
    if !tail.is_empty() {
        parts.push(PathBuf::from(tail));
    }
    parts
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GradleJavaCompileConfigJson {
    #[serde(default)]
    project_path: Option<String>,
    #[serde(default)]
    project_dir: Option<String>,
    #[serde(default)]
    compile_classpath: Option<Vec<String>>,
    #[serde(default)]
    test_compile_classpath: Option<Vec<String>>,
    #[serde(default)]
    main_source_roots: Option<Vec<String>>,
    #[serde(default)]
    test_source_roots: Option<Vec<String>>,
    #[serde(default)]
    main_output_dirs: Option<Vec<String>>,
    #[serde(default)]
    test_output_dirs: Option<Vec<String>>,
    #[serde(default)]
    source_compatibility: Option<String>,
    #[serde(default)]
    target_compatibility: Option<String>,
    #[serde(default)]
    toolchain_language_version: Option<String>,
    #[serde(default)]
    compile_compiler_args: Option<Vec<String>>,
    #[serde(default)]
    test_compiler_args: Option<Vec<String>>,
    #[serde(default)]
    infer_module_path: Option<bool>,
}

fn parse_gradle_java_compile_config_json(output: &str) -> Result<GradleJavaCompileConfigJson> {
    let json = extract_nova_json_block(output)?;
    serde_json::from_str(json.trim()).map_err(|e| BuildError::Parse(e.to_string()))
}

fn normalize_gradle_java_compile_config(
    parsed: GradleJavaCompileConfigJson,
    main_output_fallback: PathBuf,
    test_output_fallback: PathBuf,
) -> JavaCompileConfig {
    let GradleJavaCompileConfigJson {
        project_path: _,
        project_dir: _,
        compile_classpath,
        test_compile_classpath,
        main_source_roots,
        test_source_roots,
        main_output_dirs,
        test_output_dirs,
        source_compatibility,
        target_compatibility,
        toolchain_language_version,
        compile_compiler_args,
        test_compiler_args,
        infer_module_path,
    } = parsed;

    let mut main_output_dirs = strings_to_paths(main_output_dirs);
    dedupe_paths(&mut main_output_dirs);
    if main_output_dirs.is_empty() {
        main_output_dirs.push(main_output_fallback);
    }

    let mut test_output_dirs = strings_to_paths(test_output_dirs);
    dedupe_paths(&mut test_output_dirs);
    if test_output_dirs.is_empty() {
        test_output_dirs.push(test_output_fallback);
    }

    let main_output_dir = main_output_dirs.first().cloned();
    let test_output_dir = test_output_dirs.first().cloned();

    let mut resolved_compile_classpath = strings_to_paths(compile_classpath);
    dedupe_paths(&mut resolved_compile_classpath);

    let mut resolved_test_compile_classpath = strings_to_paths(test_compile_classpath);
    dedupe_paths(&mut resolved_test_compile_classpath);

    let mut compile_classpath = Vec::new();
    compile_classpath.extend(main_output_dirs.clone());
    compile_classpath.extend(resolved_compile_classpath.clone());
    dedupe_paths(&mut compile_classpath);

    let mut test_classpath = Vec::new();
    test_classpath.extend(test_output_dirs);
    test_classpath.extend(main_output_dirs);
    test_classpath.extend(resolved_test_compile_classpath);
    dedupe_paths(&mut test_classpath);

    let mut main_source_roots = strings_to_paths(main_source_roots);
    let mut test_source_roots = strings_to_paths(test_source_roots);
    dedupe_paths(&mut main_source_roots);
    dedupe_paths(&mut test_source_roots);

    let enable_preview = compile_compiler_args
        .as_deref()
        .is_some_and(compiler_args_enable_preview)
        || test_compiler_args
            .as_deref()
            .is_some_and(compiler_args_enable_preview);

    let should_infer_module_path = infer_module_path == Some(true)
        || compile_compiler_args
            .as_deref()
            .is_some_and(compiler_args_looks_like_jpms)
        || main_source_roots_have_module_info(&main_source_roots);

    let module_path = if should_infer_module_path {
        infer_module_path_entries(&resolved_compile_classpath)
    } else {
        Vec::new()
    };

    JavaCompileConfig {
        compile_classpath,
        test_classpath,
        module_path,
        main_source_roots,
        test_source_roots,
        main_output_dir,
        test_output_dir,
        source: source_compatibility,
        target: target_compatibility,
        release: toolchain_language_version,
        enable_preview,
    }
}

fn compiler_args_enable_preview(args: &[String]) -> bool {
    args.iter().any(|arg| arg.trim() == "--enable-preview")
}

fn extract_nova_json_block(output: &str) -> Result<String> {
    extract_sentinel_block(output, NOVA_JSON_BEGIN, NOVA_JSON_END)
        .ok_or_else(|| BuildError::Parse("failed to locate Gradle JSON block".into()))
}

fn strings_to_paths(value: Option<Vec<String>>) -> Vec<PathBuf> {
    value
        .unwrap_or_default()
        .into_iter()
        .filter_map(|s| {
            let s = s.trim();
            if s.is_empty() {
                None
            } else {
                Some(PathBuf::from(s))
            }
        })
        .collect()
}

fn dedupe_paths(paths: &mut Vec<PathBuf>) {
    let mut seen = HashSet::new();
    paths.retain(|p| seen.insert(p.clone()));
}

fn extract_sentinel_block(output: &str, begin: &str, end: &str) -> Option<String> {
    let mut in_block = false;
    let mut lines = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if !in_block {
            if trimmed == begin {
                in_block = true;
            }
            continue;
        }

        if trimmed == end {
            return Some(lines.join("\n"));
        }
        lines.push(line);
    }
    None
}

#[derive(Debug, Deserialize)]
struct GradleProjectsJson {
    projects: Vec<GradleProjectJson>,
}

#[derive(Debug, Deserialize)]
struct GradleProjectJson {
    path: String,
    #[serde(rename = "projectDir")]
    project_dir: String,
}

#[derive(Debug, Deserialize)]
struct GradleAllJavaCompileConfigsJson {
    projects: Vec<GradleAllJavaCompileConfigProjectJson>,
}

#[derive(Debug, Deserialize)]
struct GradleAllJavaCompileConfigProjectJson {
    path: String,
    #[serde(rename = "projectDir")]
    project_dir: String,
    config: GradleJavaCompileConfigJson,
}

fn write_init_script(project_root: &Path) -> Result<PathBuf> {
    let mut path = std::env::temp_dir();
    let token = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    path.push(format!("nova_gradle_init_{token}.gradle"));

    // Best-effort init script that registers tasks for emitting:
    // - resolved `compileClasspath` configuration entries per project
    // - Gradle project list + directories for multi-module discovery
    let script = r#"
import groovy.json.JsonOutput

def novaJavaCompileModel = { task ->
    def opts = task.options

    def apPath = []
    try {
        if (opts.annotationProcessorPath != null) {
            apPath = opts.annotationProcessorPath.files.collect { it.absolutePath }
        }
    } catch (Throwable ignored) {}

    def args = []
    try {
        args = opts.compilerArgs ?: []
    } catch (Throwable ignored) {}

    def genDir = null
    try {
        if (opts.hasProperty("generatedSourceOutputDirectory") && opts.generatedSourceOutputDirectory != null) {
            def dirProp = opts.generatedSourceOutputDirectory
            try {
                genDir = dirProp.asFile.get().absolutePath
            } catch (Throwable ignored2) {}
        } else if (opts.hasProperty("annotationProcessorGeneratedSourcesDirectory") && opts.annotationProcessorGeneratedSourcesDirectory != null) {
            genDir = opts.annotationProcessorGeneratedSourcesDirectory.absolutePath
        }
    } catch (Throwable ignored) {}

    return [annotationProcessorPath: apPath, compilerArgs: args, generatedSourcesDir: genDir]
}

def novaJavaCompileConfigPayload = { proj ->
    def payload = [:]
    payload.projectPath = proj.path
    payload.projectDir = proj.projectDir.absolutePath

    def cfg = proj.configurations.findByName("compileClasspath")
    if (cfg == null) {
        cfg = proj.configurations.findByName("runtimeClasspath")
    }

    def testCfg = proj.configurations.findByName("testCompileClasspath")
    if (testCfg == null) {
        testCfg = proj.configurations.findByName("testRuntimeClasspath")
    }
    if (testCfg == null) {
        testCfg = proj.configurations.findByName("runtimeClasspath")
    }

    payload.compileClasspath = (cfg != null) ? cfg.resolve().collect { it.absolutePath } : null
    payload.testCompileClasspath = (testCfg != null) ? testCfg.resolve().collect { it.absolutePath } : null

    def sourceSets = null
    try {
        sourceSets = proj.extensions.findByName("sourceSets")
    } catch (Exception ignored) {}

     if (sourceSets != null) {
         def main = sourceSets.findByName("main")
         def test = sourceSets.findByName("test")
         payload.mainSourceRoots = (main != null) ? main.java.srcDirs.collect { it.absolutePath } : null
         payload.testSourceRoots = (test != null) ? test.java.srcDirs.collect { it.absolutePath } : null
         payload.mainOutputDirs = (main != null) ? main.output.classesDirs.files.collect { it.absolutePath } : null
         payload.testOutputDirs = (test != null) ? test.output.classesDirs.files.collect { it.absolutePath } : null
     } else {
         payload.mainSourceRoots = null
         payload.testSourceRoots = null
         payload.mainOutputDirs = null
         payload.testOutputDirs = null
     }

     payload.compileCompilerArgs = null
     payload.testCompilerArgs = null
     payload.inferModulePath = null
 
     try {
         def t = proj.tasks.findByName("compileJava")
         if (t instanceof org.gradle.api.tasks.compile.JavaCompile) {
             try {
                 payload.compileCompilerArgs = t.options.compilerArgs
             } catch (Throwable ignored) {}
             try {
                 payload.inferModulePath = t.modularity.inferModulePath
             } catch (Throwable ignored) {}
         }
     } catch (Throwable ignored) {}
 
     try {
         def t = proj.tasks.findByName("compileTestJava")
         if (t instanceof org.gradle.api.tasks.compile.JavaCompile) {
             try {
                 payload.testCompilerArgs = t.options.compilerArgs
             } catch (Throwable ignored) {}
         }
     } catch (Throwable ignored) {}
 
     def sourceCompat = null
     def targetCompat = null
     def toolchainLang = null

    def javaExt = null
    try {
        javaExt = proj.extensions.findByName("java")
    } catch (Exception ignored) {}

    if (javaExt != null) {
        try {
            sourceCompat = javaExt.sourceCompatibility?.toString()
        } catch (Exception ignored) {}
        try {
            targetCompat = javaExt.targetCompatibility?.toString()
        } catch (Exception ignored) {}
        try {
            def lv = javaExt.toolchain?.languageVersion
            if (lv != null && lv.isPresent()) {
                toolchainLang = lv.get().asInt().toString()
            }
        } catch (Exception ignored) {}
    } else {
        try {
            sourceCompat = proj.sourceCompatibility?.toString()
        } catch (Exception ignored) {}
        try {
            targetCompat = proj.targetCompatibility?.toString()
        } catch (Exception ignored) {}
    }

    payload.sourceCompatibility = sourceCompat
    payload.targetCompatibility = targetCompat
    payload.toolchainLanguageVersion = toolchainLang

    return payload
}

allprojects { proj ->
    proj.tasks.register("printNovaJavaCompileConfig") {
        doLast {
            def payload = novaJavaCompileConfigPayload(proj)

            println("NOVA_JSON_BEGIN")
            println(JsonOutput.toJson(payload))
            println("NOVA_JSON_END")
        }
    }

    proj.tasks.register("printNovaAnnotationProcessing") {
         doLast {
             def out = [:]
             out.projectPath = proj.path
             out.projectDir = proj.projectDir.absolutePath
             def mainTask = proj.tasks.findByName("compileJava")
             if (mainTask instanceof org.gradle.api.tasks.compile.JavaCompile) {
                 out.main = novaJavaCompileModel(mainTask)
             }
            def testTask = proj.tasks.findByName("compileTestJava")
            if (testTask instanceof org.gradle.api.tasks.compile.JavaCompile) {
                out.test = novaJavaCompileModel(testTask)
            }
            println("NOVA_APT_BEGIN")
            println(JsonOutput.toJson(out))
            println("NOVA_APT_END")
        }
    }

    if (proj == proj.rootProject) {
        proj.tasks.register("printNovaProjects") {
            doLast {
                def projects = proj.rootProject.allprojects.collect { p ->
                    [path: p.path, projectDir: p.projectDir.absolutePath]
                }
                projects.sort { a, b -> a.path <=> b.path }
                def json = JsonOutput.toJson([projects: projects])
                println("NOVA_PROJECTS_BEGIN")
                println(json)
                println("NOVA_PROJECTS_END")
            }
        }

        proj.tasks.register("printNovaAllJavaCompileConfigs") {
            doLast {
                def projects = proj.rootProject.allprojects.collect { p ->
                    [path: p.path, projectDir: p.projectDir.absolutePath, config: novaJavaCompileConfigPayload(p)]
                }
                projects.sort { a, b -> a.path <=> b.path }
                def json = JsonOutput.toJson([projects: projects])
                println("NOVA_ALL_JSON_BEGIN")
                println(json)
                println("NOVA_ALL_JSON_END")
            }
        }
    }
}
"#;

    std::fs::write(&path, script)?;

    // Make sure the temp file is unique within the project (e.g. when running
    // with restrictive tmpfs setups).
    if !path.exists() {
        return Err(BuildError::Unsupported(format!(
            "failed to create init script under {}",
            project_root.display()
        )));
    }

    Ok(path)
}

fn write_compile_all_java_init_script(project_root: &Path) -> Result<PathBuf> {
    let mut path = std::env::temp_dir();
    let token = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    path.push(format!("nova_gradle_compile_all_{token}.gradle"));

    // Register a root task that depends on all `compileJava` tasks we can find.
    //
    // This is necessary for multi-project Gradle workspaces where the root
    // project is just an aggregator and does not apply the Java plugin.
    let script = r#"
gradle.rootProject { root ->
    def novaTaskProvider = root.tasks.register("novaCompileAllJava") {
        group = "build"
        description = "Compiles all Java sources across all projects (Nova helper task)"
    }

    gradle.projectsEvaluated {
        def compileTasks = []
        root.allprojects { proj ->
            def t = proj.tasks.findByName("compileJava")
            if (t != null) {
                compileTasks.add(t)
            }
        }
        novaTaskProvider.configure {
            dependsOn compileTasks
        }
    }
}
"#;

    std::fs::write(&path, script)?;

    if !path.exists() {
        return Err(BuildError::Unsupported(format!(
            "failed to create init script under {}",
            project_root.display()
        )));
    }

    Ok(path)
}

fn write_compile_all_test_java_init_script(project_root: &Path) -> Result<PathBuf> {
    let mut path = std::env::temp_dir();
    let token = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    path.push(format!("nova_gradle_compile_all_test_{token}.gradle"));

    // Register a root task that depends on all `compileTestJava` tasks we can find.
    //
    // Similar to `write_compile_all_java_init_script`, this helps in multi-project
    // Gradle builds where the root project is an aggregator.
    let script = r#"
gradle.rootProject { root ->
    def novaTaskProvider = root.tasks.register("novaCompileAllTestJava") {
        group = "build"
        description = "Compiles all Java test sources across all projects (Nova helper task)"
    }

    gradle.projectsEvaluated {
        def compileTasks = []
        root.allprojects { proj ->
            def t = proj.tasks.findByName("compileTestJava")
            if (t != null) {
                compileTasks.add(t)
            }
        }
        novaTaskProvider.configure {
            dependsOn compileTasks
        }
    }
}
"#;

    std::fs::write(&path, script)?;

    if !path.exists() {
        return Err(BuildError::Unsupported(format!(
            "failed to create init script under {}",
            project_root.display()
        )));
    }

    Ok(path)
}

pub fn collect_gradle_build_files(root: &Path) -> Result<Vec<PathBuf>> {
    Ok(nova_build_model::collect_gradle_build_files(root)?)
}

// -----------------------------------------------------------------------------
// Gradle build model snapshot handoff (`.nova/queries/gradle.json`).
// -----------------------------------------------------------------------------

static GRADLE_SNAPSHOT_TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn gradle_snapshot_path(project_root: &Path) -> PathBuf {
    project_root.join(GRADLE_SNAPSHOT_REL_PATH)
}

fn update_gradle_snapshot_projects(
    project_root: &Path,
    fingerprint: &BuildFileFingerprint,
    projects: &[GradleProjectInfo],
) -> std::io::Result<()> {
    update_gradle_snapshot(project_root, fingerprint, |snapshot| {
        let mut snapshot_projects: Vec<GradleSnapshotProject> = projects
            .iter()
            .map(|p| GradleSnapshotProject {
                path: p.path.clone(),
                project_dir: p.dir.clone(),
            })
            .collect();
        snapshot_projects.sort_by(|a, b| a.path.cmp(&b.path));
        snapshot_projects.dedup_by(|a, b| a.path == b.path);
        snapshot.projects = snapshot_projects;

        // Keep compile config `projectDir` in sync with the authoritative mapping.
        let dir_map: HashMap<String, PathBuf> = snapshot
            .projects
            .iter()
            .map(|p| (p.path.clone(), p.project_dir.clone()))
            .collect();
        for (path, cfg) in snapshot.java_compile_configs.iter_mut() {
            if let Some(dir) = dir_map.get(path) {
                cfg.project_dir = dir.clone();
            }
        }
    })
}

fn update_gradle_snapshot_java_compile_config(
    project_root: &Path,
    fingerprint: &BuildFileFingerprint,
    project_path: &str,
    project_dir: &Path,
    config: &JavaCompileConfig,
) -> std::io::Result<()> {
    update_gradle_snapshot(project_root, fingerprint, |snapshot| {
        snapshot.java_compile_configs.insert(
            project_path.to_string(),
            GradleSnapshotJavaCompileConfig {
                project_dir: project_dir.to_path_buf(),
                compile_classpath: config.compile_classpath.clone(),
                test_classpath: config.test_classpath.clone(),
                module_path: config.module_path.clone(),
                main_source_roots: config.main_source_roots.clone(),
                test_source_roots: config.test_source_roots.clone(),
                main_output_dir: config.main_output_dir.clone(),
                test_output_dir: config.test_output_dir.clone(),
                source: config.source.clone(),
                target: config.target.clone(),
                release: config.release.clone(),
                enable_preview: config.enable_preview,
            },
        );

        // If the projects mapping is missing, best-effort: record at least the known project.
        if !snapshot.projects.iter().any(|p| p.path == project_path) {
            snapshot.projects.push(GradleSnapshotProject {
                path: project_path.to_string(),
                project_dir: project_dir.to_path_buf(),
            });
        }
    })
}

fn update_gradle_snapshot(
    project_root: &Path,
    fingerprint: &BuildFileFingerprint,
    update: impl FnOnce(&mut GradleSnapshotFile),
) -> std::io::Result<()> {
    let path = gradle_snapshot_path(project_root);
    let mut snapshot = read_gradle_snapshot_file(&path).unwrap_or_default();
    if snapshot.schema_version != GRADLE_SNAPSHOT_SCHEMA_VERSION
        || snapshot.build_fingerprint != fingerprint.digest
    {
        snapshot = GradleSnapshotFile {
            schema_version: GRADLE_SNAPSHOT_SCHEMA_VERSION,
            build_fingerprint: fingerprint.digest.clone(),
            projects: Vec::new(),
            java_compile_configs: BTreeMap::new(),
        };
    }

    update(&mut snapshot);
    snapshot.schema_version = GRADLE_SNAPSHOT_SCHEMA_VERSION;
    snapshot.build_fingerprint = fingerprint.digest.clone();

    // Best-effort determinism: stable sort + dedup for any path lists.
    snapshot.projects.sort_by(|a, b| a.path.cmp(&b.path));
    snapshot.projects.dedup_by(|a, b| a.path == b.path);
    for cfg in snapshot.java_compile_configs.values_mut() {
        sort_dedup_paths(&mut cfg.compile_classpath);
        sort_dedup_paths(&mut cfg.test_classpath);
        sort_dedup_paths(&mut cfg.module_path);
        sort_dedup_paths(&mut cfg.main_source_roots);
        sort_dedup_paths(&mut cfg.test_source_roots);
    }

    let json = serde_json::to_vec_pretty(&snapshot)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    write_file_atomic(&path, &json)
}

fn read_gradle_snapshot_file(path: &Path) -> Option<GradleSnapshotFile> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn sort_dedup_paths(paths: &mut Vec<PathBuf>) {
    paths.sort();
    paths.dedup();
}

fn write_file_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    std::fs::create_dir_all(parent)?;

    let (tmp_path, mut file) = open_unique_tmp_file(path, parent)?;
    let write_result = (|| -> std::io::Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(err) = write_result {
        drop(file);
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err);
    }
    drop(file);

    if let Err(err) = rename_overwrite(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err);
    }

    #[cfg(unix)]
    {
        let _ = std::fs::File::open(parent).and_then(|dir| dir.sync_all());
    }

    Ok(())
}

fn open_unique_tmp_file(dest: &Path, parent: &Path) -> std::io::Result<(PathBuf, std::fs::File)> {
    let file_name = dest
        .file_name()
        .ok_or_else(|| std::io::Error::other("destination path has no file name"))?;
    let pid = std::process::id();

    loop {
        let counter = GRADLE_SNAPSHOT_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut tmp_name = file_name.to_os_string();
        tmp_name.push(format!(".tmp.{pid}.{counter}"));
        let tmp_path = parent.join(tmp_name);

        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
        {
            Ok(file) => return Ok((tmp_path, file)),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
}

fn rename_overwrite(src: &Path, dest: &Path) -> std::io::Result<()> {
    const MAX_RENAME_ATTEMPTS: usize = 1024;
    let mut attempts = 0usize;

    loop {
        match std::fs::rename(src, dest) {
            Ok(()) => return Ok(()),
            Err(err)
                if cfg!(windows)
                    && (err.kind() == std::io::ErrorKind::AlreadyExists || dest.exists()) =>
            {
                match std::fs::remove_file(dest) {
                    Ok(()) => {}
                    Err(remove_err) if remove_err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(remove_err) => return Err(remove_err),
                }

                attempts += 1;
                if attempts >= MAX_RENAME_ATTEMPTS {
                    return Err(err);
                }
                continue;
            }
            Err(err) => return Err(err),
        }
    }
}
