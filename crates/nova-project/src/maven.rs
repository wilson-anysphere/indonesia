use std::collections::{hash_map::Entry, BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use crate::discover::{LoadOptions, ProjectError};
use crate::{
    BuildSystem, ClasspathEntry, ClasspathEntryKind, Dependency, JavaConfig, JavaLanguageLevel,
    JavaVersion, LanguageLevelProvenance, MavenGav, Module, ModuleLanguageLevel, OutputDir,
    OutputDirKind, ProjectConfig, SourceRoot, SourceRootKind, SourceRootOrigin,
    WorkspaceModuleBuildId, WorkspaceModuleConfig, WorkspaceProjectModel,
};
use regex::Regex;
use walkdir::WalkDir;

pub(crate) fn load_maven_project(
    root: &Path,
    options: &LoadOptions,
) -> Result<ProjectConfig, ProjectError> {
    let root_pom_path = root.join("pom.xml");
    let root_pom = parse_pom(&root_pom_path)?;
    let include_root_module =
        root_pom.packaging.as_deref() != Some("pom") || root_pom.modules.is_empty();

    let maven_repo = discover_maven_repo(root, options);
    let mut resolver = MavenResolver::new(maven_repo.clone());
    resolver.cache_raw_pom(&root_pom_path, root_pom.clone());

    let mut modules = Vec::new();
    let mut source_roots = Vec::new();
    let mut output_dirs = Vec::new();
    let mut dependencies = Vec::new();
    let mut classpath = Vec::new();
    let mut dependency_entries = Vec::new();

    let root_effective = resolver
        .effective_pom_from_path(&root_pom_path)
        .unwrap_or_else(|| {
            let mut visiting = HashSet::new();
            Arc::new(EffectivePom::from_raw(
                &root_pom,
                None,
                &mut resolver,
                &mut visiting,
            ))
        });

    let mut discovered_modules =
        discover_modules_recursive(root, &root_pom, Arc::clone(&root_effective), &mut resolver)?;
    discovered_modules.sort_by(|a, b| a.root.cmp(&b.root));
    discovered_modules.dedup_by(|a, b| a.root == b.root);

    let workspace_modules =
        build_workspace_module_index(root, include_root_module, &discovered_modules);

    // Workspace-level Java config: take the maximum across modules so we don't
    // under-report language features used anywhere in the workspace.
    let mut workspace_java = root_effective.java.unwrap_or_default();

    for module in &discovered_modules {
        let module_root = &module.root;
        if module_root == root && !include_root_module {
            // When the workspace root is an aggregator POM (`packaging=pom` with `<modules>`),
            // treat the child modules as the workspace modules and avoid creating a synthetic
            // "root" module entry. This matches `workspace_root` expectations for nested loads.
            continue;
        }

        let effective = module.effective.as_ref();
        let module_java = effective
            .java
            .unwrap_or(root_effective.java.unwrap_or_default());

        if module_java.source > workspace_java.source {
            workspace_java.source = module_java.source;
        }
        if module_java.target > workspace_java.target {
            workspace_java.target = module_java.target;
        }
        workspace_java.enable_preview |= module_java.enable_preview;

        let module_display_name = if module_root == root {
            module
                .raw_pom
                .artifact_id
                .clone()
                .unwrap_or_else(|| "root".to_string())
        } else {
            module_root
                .strip_prefix(root)
                .unwrap_or(module_root)
                .to_string_lossy()
                .to_string()
        };

        modules.push(Module {
            name: module_display_name,
            root: module_root.clone(),
            annotation_processing: Default::default(),
        });

        // Maven standard source layout.
        let main_standard = push_source_root(
            &mut source_roots,
            &module_root,
            SourceRootKind::Main,
            "src/main/java",
        );
        let test_standard = push_source_root(
            &mut source_roots,
            &module_root,
            SourceRootKind::Test,
            "src/test/java",
        );

        // Some large OSS projects (e.g. Guava) still use a legacy "src/" + "test/"
        // layout in Maven modules. Fall back to those roots when the standard
        // Maven conventions are not present.
        if !main_standard {
            push_source_root_if_has_java(
                &mut source_roots,
                &module_root,
                SourceRootKind::Main,
                "src",
            );
        }
        if !test_standard {
            push_source_root_if_has_java(
                &mut source_roots,
                &module_root,
                SourceRootKind::Test,
                "test",
            );
        }

        crate::generated::append_generated_source_roots(
            &mut source_roots,
            root,
            &module_root,
            BuildSystem::Maven,
            &options.nova_config,
        );

        // Expected output directories even if they don't exist yet (pre-build).
        let main_output = module_root.join("target/classes");
        let test_output = module_root.join("target/test-classes");
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

        // Dependencies (direct + transitive, resolved from local Maven repo).
        let resolved_deps = resolver.resolve_dependency_closure(&effective.dependencies);
        for dep in &resolved_deps {
            if dep.group_id.is_empty() || dep.artifact_id.is_empty() {
                continue;
            }
            dependencies.push(dep.clone());

            if is_workspace_module_dependency(dep, &workspace_modules) {
                // Workspace module outputs are already on the classpath.
                continue;
            }

            if let Some(jar_path) = maven_dependency_jar_path(&maven_repo, dep) {
                dependency_entries.push(ClasspathEntry {
                    // Maven dependency artifacts are typically jar files, but some build systems
                    // (and test fixtures) can "explode" jars into directories (often still ending
                    // with `.jar`). Treat existing directories as directories.
                    // Missing artifacts are omitted so downstream JPMS/classpath indexing doesn't
                    // fail trying to open non-existent archives (see
                    // `tests/cases/maven_missing_jars.rs`).
                    kind: if jar_path.is_dir() {
                        ClasspathEntryKind::Directory
                    } else {
                        ClasspathEntryKind::Jar
                    },
                    path: jar_path,
                });
            }
        }
    }

    // Compute workspace Java config:
    // - prefer explicit root config
    // - otherwise default (17)
    let java = workspace_java;

    // Sort/dedup for stability.
    sort_dedup_modules(&mut modules);
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
        build_system: BuildSystem::Maven,
        java,
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

pub(crate) fn load_maven_workspace_model(
    root: &Path,
    options: &LoadOptions,
) -> Result<WorkspaceProjectModel, ProjectError> {
    let root_pom_path = root.join("pom.xml");
    let root_pom = parse_pom(&root_pom_path)?;
    let include_root_module =
        root_pom.packaging.as_deref() != Some("pom") || root_pom.modules.is_empty();

    let maven_repo = discover_maven_repo(root, options);
    let mut resolver = MavenResolver::new(maven_repo.clone());
    resolver.cache_raw_pom(&root_pom_path, root_pom.clone());

    let root_effective = resolver
        .effective_pom_from_path(&root_pom_path)
        .unwrap_or_else(|| {
            let mut visiting = HashSet::new();
            Arc::new(EffectivePom::from_raw(
                &root_pom,
                None,
                &mut resolver,
                &mut visiting,
            ))
        });

    let mut discovered_modules =
        discover_modules_recursive(root, &root_pom, Arc::clone(&root_effective), &mut resolver)?;
    discovered_modules.sort_by(|a, b| a.root.cmp(&b.root));
    discovered_modules.dedup_by(|a, b| a.root == b.root);

    let workspace_modules =
        build_workspace_module_index(root, include_root_module, &discovered_modules);

    let mut module_configs = Vec::new();
    for module in &discovered_modules {
        let module_root = &module.root;
        if module_root == root && !include_root_module {
            continue;
        }

        let effective = module.effective.as_ref();

        let module_java = effective
            .java
            .unwrap_or(root_effective.java.unwrap_or_default());

        let module_display_name = if module_root == root {
            module
                .raw_pom
                .artifact_id
                .clone()
                .unwrap_or_else(|| "root".to_string())
        } else {
            module_root
                .strip_prefix(root)
                .unwrap_or(module_root)
                .to_string_lossy()
                .to_string()
        };

        let group_id = effective.group_id.clone().unwrap_or_default();
        let artifact_id = effective
            .artifact_id
            .clone()
            .unwrap_or_else(|| module_display_name.clone());

        let module_path = if module_root == root {
            ".".to_string()
        } else {
            module_root
                .strip_prefix(root)
                .unwrap_or(module_root)
                .to_string_lossy()
                .to_string()
        };

        let id = if !group_id.is_empty() && !artifact_id.is_empty() {
            format!("maven:{group_id}:{artifact_id}")
        } else if module_root == root {
            "maven:root".to_string()
        } else {
            format!("maven:path:{module_path}")
        };

        let module_pom_path = module_root.join("pom.xml");
        let java_provenance = if pom_declares_java_config(&module.raw_pom) {
            LanguageLevelProvenance::BuildFile(module_pom_path)
        } else if root_effective.java.is_some() {
            LanguageLevelProvenance::BuildFile(root_pom_path.clone())
        } else {
            LanguageLevelProvenance::Default
        };

        let language_level = ModuleLanguageLevel {
            level: JavaLanguageLevel::from_java_config(module_java),
            provenance: java_provenance,
        };

        let build_id = WorkspaceModuleBuildId::Maven {
            module_path,
            gav: MavenGav {
                group_id,
                artifact_id,
                version: effective.version.clone(),
            },
        };

        let mut source_roots = Vec::new();
        push_source_root(
            &mut source_roots,
            module_root,
            SourceRootKind::Main,
            "src/main/java",
        );
        push_source_root(
            &mut source_roots,
            module_root,
            SourceRootKind::Test,
            "src/test/java",
        );
        crate::generated::append_generated_source_roots(
            &mut source_roots,
            root,
            module_root,
            BuildSystem::Maven,
            &options.nova_config,
        );

        let main_output = module_root.join("target/classes");
        let test_output = module_root.join("target/test-classes");
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

        let dependencies = effective
            .dependencies
            .iter()
            .map(|d| d.as_public())
            .collect::<Vec<_>>();

        // Maven dependencies are transitive by default. When a module depends on another
        // workspace module, we want to include both:
        // - the dependency module's output directory (reactor build output), and
        // - the dependency module's external dependencies on the consuming module's classpath.
        //
        // This traversal expands workspace module dependencies into an "effective root"
        // dependency list that includes external deps contributed by workspace modules.
        let mut expanded_pom_dependencies = effective.dependencies.clone();
        let mut workspace_module_exclusions: HashMap<(String, String), BTreeSet<(String, String)>> =
            HashMap::new();

        let mut queue: VecDeque<(PomDependency, BTreeSet<(String, String)>)> = effective
            .dependencies
            .iter()
            .cloned()
            .map(|dep| (dep.clone(), dep.exclusions.clone()))
            .collect();

        while let Some((dep, exclusions)) = queue.pop_front() {
            let key = dep.ga();
            let Some(info) = workspace_modules.get(&key) else {
                continue;
            };
            if !versions_compatible(dep.version.as_deref(), info.version.as_deref()) {
                continue;
            }

            // Keep an intersection of exclusion sets across all discovered paths to a workspace
            // module, mirroring MavenResolver's best-effort "don't over-exclude" behavior.
            let should_expand = match workspace_module_exclusions.entry(key.clone()) {
                Entry::Vacant(v) => {
                    v.insert(exclusions.clone());
                    true
                }
                Entry::Occupied(mut o) => {
                    let intersection = exclusion_intersection(o.get(), &exclusions);
                    if intersection == *o.get() {
                        false
                    } else {
                        o.insert(intersection);
                        true
                    }
                }
            };

            if !should_expand {
                continue;
            }

            let exclusions = workspace_module_exclusions
                .get(&key)
                .cloned()
                .unwrap_or_default();

            classpath.push(ClasspathEntry {
                kind: ClasspathEntryKind::Directory,
                path: info.root.join("target/classes"),
            });

            // Best-effort: if the dependency module POM couldn't be parsed (or had no
            // dependencies), this list will be empty and we just won't expand further.
            for child in &info.dependencies {
                // Optional dependencies do not propagate transitively.
                if child.optional {
                    continue;
                }

                let child_ga = child.ga();
                if exclusion_matches(&exclusions, &child_ga.0, &child_ga.1) {
                    continue;
                }

                let mut child_dep = child.clone();
                child_dep.exclusions.extend(exclusions.iter().cloned());
                normalize_exclusions(&mut child_dep.exclusions);

                expanded_pom_dependencies.push(child_dep.clone());
                queue.push_back((child_dep.clone(), child_dep.exclusions.clone()));
            }
        }

        // Resolve direct + transitive external dependencies from local Maven repo, using the
        // expanded dependency list so workspace module dependencies contribute their external
        // dependency closure.
        let resolved_deps = resolver.resolve_dependency_closure(&expanded_pom_dependencies);
        for dep in &resolved_deps {
            if dep.group_id.is_empty() || dep.artifact_id.is_empty() {
                continue;
            }

            if is_workspace_module_dependency(dep, &workspace_modules) {
                // Use workspace output directories instead of `.m2` jar placeholders.
                continue;
            }

            if let Some(jar_path) = maven_dependency_jar_path(&maven_repo, dep) {
                classpath.push(ClasspathEntry {
                    // Support "exploded jar" directories on disk; omit missing artifacts so
                    // downstream indexing doesn't fail trying to open them.
                    kind: if jar_path.is_dir() {
                        ClasspathEntryKind::Directory
                    } else {
                        ClasspathEntryKind::Jar
                    },
                    path: jar_path,
                });
            }
        }

        sort_dedup_source_roots(&mut source_roots);
        sort_dedup_output_dirs(&mut output_dirs);
        sort_dedup_classpath(&mut classpath);
        let mut dependencies = dependencies;
        sort_dedup_dependencies(&mut dependencies);

        module_configs.push(WorkspaceModuleConfig {
            id,
            name: module_display_name,
            root: module_root.clone(),
            build_id,
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
            annotation_processing: Default::default(),
        })
        .collect::<Vec<_>>();
    let jpms_modules = crate::jpms::discover_jpms_modules(&modules_for_jpms);

    // JPMS-aware workspace model: when the workspace contains any `module-info.java`, treat Maven
    // dependency jars as module-path entries so downstream consumers can resolve named modules.
    //
    // This intentionally bypasses `jpms::classify_dependency_entries`'s "stable module name"
    // heuristic: `WorkspaceProjectModel` is used to build a module-aware classpath index, and we
    // want *all* jar dependencies to be indexed as module-path candidates when JPMS is enabled.
    if crate::jpms::workspace_uses_jpms(&jpms_modules) {
        for module in &mut module_configs {
            let mut module_path = std::mem::take(&mut module.module_path);
            let mut classpath = Vec::new();

            for entry in std::mem::take(&mut module.classpath) {
                let is_archive = entry
                    .path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| {
                        ext.eq_ignore_ascii_case("jar") || ext.eq_ignore_ascii_case("jmod")
                    });

                if entry.kind == ClasspathEntryKind::Jar || is_archive {
                    module_path.push(entry)
                } else {
                    classpath.push(entry)
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
        BuildSystem::Maven,
        root_effective.java.unwrap_or_default(),
        module_configs,
        jpms_modules,
    ))
}

#[derive(Debug, Clone)]
struct DiscoveredModule {
    root: PathBuf,
    raw_pom: RawPom,
    effective: Arc<EffectivePom>,
}

fn discover_modules_recursive(
    workspace_root: &Path,
    root_pom: &RawPom,
    root_effective: Arc<EffectivePom>,
    resolver: &mut MavenResolver,
) -> Result<Vec<DiscoveredModule>, ProjectError> {
    let mut visited: HashSet<PathBuf> = HashSet::new();
    // `workspace_root` is canonicalized by `load_project_with_options`.
    visited.insert(workspace_root.to_path_buf());

    let mut out = vec![DiscoveredModule {
        root: workspace_root.to_path_buf(),
        raw_pom: root_pom.clone(),
        effective: Arc::clone(&root_effective),
    }];
    let mut queue: VecDeque<PathBuf> = VecDeque::new();

    let mut root_modules = root_pom.modules.clone();
    root_modules.sort();
    root_modules.dedup();
    for module in root_modules {
        queue.push_back(workspace_root.join(module));
    }

    while let Some(module_root) = queue.pop_front() {
        let module_root = canonicalize_or_fallback(&module_root);
        if !visited.insert(module_root.clone()) {
            continue;
        }

        let module_pom_path = module_root.join("pom.xml");
        let raw_pom = if module_pom_path.is_file() {
            // Module discovery should be best-effort; some workspaces have missing or invalid
            // POM files for optional modules (e.g. profile-only modules).
            match parse_pom(&module_pom_path) {
                Ok(raw) => {
                    resolver.cache_raw_pom(&module_pom_path, raw.clone());
                    raw
                }
                Err(_) => RawPom::default(),
            }
        } else {
            RawPom::default()
        };

        let effective = if module_pom_path.is_file() {
            resolver
                .effective_pom_from_path(&module_pom_path)
                .unwrap_or_else(|| {
                    let mut visiting = HashSet::new();
                    Arc::new(EffectivePom::from_raw(
                        &raw_pom,
                        None,
                        resolver,
                        &mut visiting,
                    ))
                })
        } else {
            let mut visiting = HashSet::new();
            Arc::new(EffectivePom::from_raw(
                &raw_pom,
                None,
                resolver,
                &mut visiting,
            ))
        };

        let mut child_modules = raw_pom.modules.clone();
        child_modules.sort();
        child_modules.dedup();
        for child in child_modules {
            queue.push_back(module_root.join(child));
        }

        out.push(DiscoveredModule {
            root: module_root,
            raw_pom,
            effective,
        });
    }

    Ok(out)
}

fn canonicalize_or_fallback(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[derive(Debug, Default, Clone)]
struct RawPom {
    group_id: Option<String>,
    artifact_id: Option<String>,
    version: Option<String>,
    packaging: Option<String>,
    properties: BTreeMap<String, String>,
    compiler_plugin: Option<RawMavenCompilerPluginConfig>,
    dependencies: Vec<PomDependency>,
    dependency_management: Vec<PomDependency>,
    modules: Vec<String>,
    parent: Option<PomParent>,
    profiles: Vec<RawProfile>,
}

#[derive(Debug, Default, Clone)]
struct RawMavenCompilerPluginConfig {
    release: Option<String>,
    source: Option<String>,
    target: Option<String>,
    compiler_args: Vec<String>,
}

impl RawMavenCompilerPluginConfig {
    fn is_empty(&self) -> bool {
        self.release.is_none()
            && self.source.is_none()
            && self.target.is_none()
            && self.compiler_args.is_empty()
    }

    fn merge(&mut self, other: RawMavenCompilerPluginConfig) {
        if other.release.is_some() {
            self.release = other.release;
        }
        if other.source.is_some() {
            self.source = other.source;
        }
        if other.target.is_some() {
            self.target = other.target;
        }
        self.compiler_args.extend(other.compiler_args);
    }

    fn enable_preview(&self, props: &BTreeMap<String, String>) -> bool {
        self.compiler_args.iter().any(|arg| {
            let resolved = resolve_placeholders(arg, props);
            resolved.trim() == "--enable-preview"
        })
    }

    fn release(&self, props: &BTreeMap<String, String>) -> Option<JavaVersion> {
        self.release
            .as_deref()
            .map(|v| resolve_placeholders(v, props))
            .and_then(|v| JavaVersion::parse(&v))
    }

    fn source(&self, props: &BTreeMap<String, String>) -> Option<JavaVersion> {
        self.source
            .as_deref()
            .map(|v| resolve_placeholders(v, props))
            .and_then(|v| JavaVersion::parse(&v))
    }

    fn target(&self, props: &BTreeMap<String, String>) -> Option<JavaVersion> {
        self.target
            .as_deref()
            .map(|v| resolve_placeholders(v, props))
            .and_then(|v| JavaVersion::parse(&v))
    }
}

#[derive(Debug, Clone)]
struct PomParent {
    group_id: Option<String>,
    artifact_id: Option<String>,
    version: Option<String>,
    relative_path: Option<String>,
}

#[derive(Debug, Default, Clone)]
struct RawProfile {
    active_by_default: bool,
    properties: BTreeMap<String, String>,
    dependencies: Vec<PomDependency>,
    dependency_management: Vec<PomDependency>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PomDependency {
    group_id: String,
    artifact_id: String,
    version: Option<String>,
    scope: Option<String>,
    classifier: Option<String>,
    type_: Option<String>,
    optional: bool,
    optional_specified: bool,
    exclusions: BTreeSet<(String, String)>,
}

impl PomDependency {
    fn ga(&self) -> (String, String) {
        (self.group_id.clone(), self.artifact_id.clone())
    }

    fn as_public(&self) -> Dependency {
        Dependency {
            group_id: self.group_id.clone(),
            artifact_id: self.artifact_id.clone(),
            version: self.version.clone(),
            scope: self.scope.clone(),
            classifier: self.classifier.clone(),
            type_: self.type_.clone(),
        }
    }
}

fn normalize_exclusions(exclusions: &mut BTreeSet<(String, String)>) {
    if exclusions.contains(&("*".to_string(), "*".to_string())) {
        exclusions.clear();
        exclusions.insert(("*".to_string(), "*".to_string()));
        return;
    }

    let group_wildcards: BTreeSet<String> = exclusions
        .iter()
        .filter(|(_, artifact_id)| artifact_id.as_str() == "*")
        .map(|(group_id, _)| group_id.clone())
        .collect();

    let artifact_wildcards: BTreeSet<String> = exclusions
        .iter()
        .filter(|(group_id, _)| group_id.as_str() == "*")
        .map(|(_, artifact_id)| artifact_id.clone())
        .collect();

    exclusions.retain(|(group_id, artifact_id)| {
        if group_id.as_str() == "*" || artifact_id.as_str() == "*" {
            return true;
        }
        !group_wildcards.contains(group_id) && !artifact_wildcards.contains(artifact_id)
    });
}

fn exclusion_matches(
    exclusions: &BTreeSet<(String, String)>,
    group_id: &str,
    artifact_id: &str,
) -> bool {
    exclusions
        .iter()
        .any(|(g, a)| (g == "*" || g == group_id) && (a == "*" || a == artifact_id))
}

fn exclusion_intersection(
    a: &BTreeSet<(String, String)>,
    b: &BTreeSet<(String, String)>,
) -> BTreeSet<(String, String)> {
    fn intersect_pattern(left: (&str, &str), right: (&str, &str)) -> Option<(String, String)> {
        let (g1, a1) = left;
        let (g2, a2) = right;

        let group = if g1 == "*" {
            g2
        } else if g2 == "*" {
            g1
        } else if g1 == g2 {
            g1
        } else {
            return None;
        };

        let artifact = if a1 == "*" {
            a2
        } else if a2 == "*" {
            a1
        } else if a1 == a2 {
            a1
        } else {
            return None;
        };

        Some((group.to_string(), artifact.to_string()))
    }

    let mut out = BTreeSet::new();
    for (ag, aa) in a {
        for (bg, ba) in b {
            if let Some(intersection) =
                intersect_pattern((ag.as_str(), aa.as_str()), (bg.as_str(), ba.as_str()))
            {
                out.insert(intersection);
            }
        }
    }
    normalize_exclusions(&mut out);
    out
}

#[derive(Debug, Clone)]
struct EffectivePom {
    group_id: Option<String>,
    artifact_id: Option<String>,
    version: Option<String>,
    java: Option<JavaConfig>,
    properties: BTreeMap<String, String>,
    compiler_plugin: Option<RawMavenCompilerPluginConfig>,
    dependency_management: BTreeMap<(String, String), PomDependency>,
    dependencies: Vec<PomDependency>,
}

impl EffectivePom {
    fn from_raw(
        raw: &RawPom,
        parent: Option<&EffectivePom>,
        resolver: &mut MavenResolver,
        visiting: &mut HashSet<PathBuf>,
    ) -> Self {
        let raw = raw.with_active_profiles_applied();

        let group_id = raw
            .group_id
            .clone()
            .or_else(|| raw.parent.as_ref().and_then(|p| p.group_id.clone()))
            .or_else(|| parent.and_then(|p| p.group_id.clone()));
        let artifact_id = raw
            .artifact_id
            .clone()
            .or_else(|| raw.parent.as_ref().and_then(|p| p.artifact_id.clone()))
            .or_else(|| parent.and_then(|p| p.artifact_id.clone()));
        let version = raw
            .version
            .clone()
            .or_else(|| raw.parent.as_ref().and_then(|p| p.version.clone()))
            .or_else(|| parent.and_then(|p| p.version.clone()));

        let mut properties = parent.map(|p| p.properties.clone()).unwrap_or_default();
        properties.extend(raw.properties.clone());

        if let Some(v) = group_id.as_ref() {
            properties.insert("project.groupId".to_string(), v.clone());
            properties.insert("pom.groupId".to_string(), v.clone());
        }
        if let Some(v) = artifact_id.as_ref() {
            properties.insert("project.artifactId".to_string(), v.clone());
            properties.insert("pom.artifactId".to_string(), v.clone());
        }
        if let Some(v) = version.as_ref() {
            properties.insert("project.version".to_string(), v.clone());
            properties.insert("pom.version".to_string(), v.clone());
        }

        // Parent properties are commonly referenced.
        if let Some(parent_coords) = raw.parent.as_ref() {
            if let Some(v) = parent_coords.group_id.as_ref() {
                properties.insert("project.parent.groupId".to_string(), v.clone());
            }
            if let Some(v) = parent_coords.artifact_id.as_ref() {
                properties.insert("project.parent.artifactId".to_string(), v.clone());
            }
            if let Some(v) = parent_coords.version.as_ref() {
                properties.insert("project.parent.version".to_string(), v.clone());
            }
        } else if let Some(parent) = parent {
            if let Some(v) = parent.group_id.as_ref() {
                properties.insert("project.parent.groupId".to_string(), v.clone());
            }
            if let Some(v) = parent.artifact_id.as_ref() {
                properties.insert("project.parent.artifactId".to_string(), v.clone());
            }
            if let Some(v) = parent.version.as_ref() {
                properties.insert("project.parent.version".to_string(), v.clone());
            }
        }

        let mut compiler_plugin = parent.and_then(|p| p.compiler_plugin.clone());
        if let Some(raw_config) = raw.compiler_plugin.clone() {
            match compiler_plugin.as_mut() {
                Some(existing) => existing.merge(raw_config),
                None => compiler_plugin = Some(raw_config),
            }
        }

        // Resolve Java config after properties are merged.
        let java = java_from_maven_config(&properties, compiler_plugin.as_ref())
            .or_else(|| parent.and_then(|p| p.java));

        // Preserve raw placeholders in dependency management, but re-key inherited entries using
        // the current module's merged properties. This matches Maven interpolation semantics where
        // inherited values (including their placeholders) are resolved in the context of the
        // child module.
        let mut dependency_management = BTreeMap::new();
        if let Some(parent) = parent {
            for dep in parent.dependency_management.values() {
                let key = (
                    resolve_placeholders(&dep.group_id, &properties),
                    resolve_placeholders(&dep.artifact_id, &properties),
                );
                dependency_management.insert(key, dep.clone());
            }
        }

        // Apply imported BOMs (in order) before this module's own managed deps.
        let mut imported_boms = Vec::new();
        let mut local_managed = Vec::new();
        for dep in &raw.dependency_management {
            let dep = dep.clone();
            if is_bom_import(&dep) {
                imported_boms.push(dep);
            } else {
                local_managed.push(dep);
            }
        }

        for bom in imported_boms {
            let group_id = resolve_placeholders(&bom.group_id, &properties);
            let artifact_id = resolve_placeholders(&bom.artifact_id, &properties);
            if group_id.is_empty() || artifact_id.is_empty() {
                continue;
            }

            let mut version = bom
                .version
                .as_deref()
                .map(|v| resolve_placeholders(v, &properties))
                .or_else(|| {
                    dependency_management
                        .get(&(group_id.clone(), artifact_id.clone()))
                        .and_then(|managed| managed.version.as_deref())
                        .map(|v| resolve_placeholders(v, &properties))
                });

            if version
                .as_deref()
                .is_some_and(|v| v.contains("${") || v.trim().is_empty())
            {
                version = None;
            }

            let Some(version) = version else {
                continue;
            };

            let Some(bom_effective) =
                resolver.effective_pom_from_gav_inner(&group_id, &artifact_id, &version, visiting)
            else {
                continue;
            };

            // Imported BOM-managed deps should be resolved in the BOM's property context.
            for (k, v) in &bom_effective.dependency_management {
                let mut managed = v.clone();
                managed.version = managed
                    .version
                    .as_deref()
                    .map(|v| resolve_placeholders(v, &bom_effective.properties));
                dependency_management.insert(k.clone(), managed);
            }
        }

        for dep in local_managed {
            let key = (
                resolve_placeholders(&dep.group_id, &properties),
                resolve_placeholders(&dep.artifact_id, &properties),
            );
            dependency_management.insert(key, dep);
        }

        let mut dependencies = Vec::new();
        for dep in &raw.dependencies {
            let mut dep = dep.clone();

            dep.group_id = resolve_placeholders(&dep.group_id, &properties);
            dep.artifact_id = resolve_placeholders(&dep.artifact_id, &properties);
            dep.exclusions = dep
                .exclusions
                .iter()
                .map(|(group_id, artifact_id)| {
                    (
                        resolve_placeholders(group_id, &properties),
                        resolve_placeholders(artifact_id, &properties),
                    )
                })
                .filter(|(group_id, artifact_id)| !group_id.is_empty() && !artifact_id.is_empty())
                .collect();
            normalize_exclusions(&mut dep.exclusions);
            dep.scope = dep
                .scope
                .as_deref()
                .map(|v| resolve_placeholders(v, &properties));
            dep.classifier = dep
                .classifier
                .as_deref()
                .map(|v| resolve_placeholders(v, &properties));
            dep.type_ = dep
                .type_
                .as_deref()
                .map(|v| resolve_placeholders(v, &properties));

            let managed =
                dependency_management.get(&(dep.group_id.clone(), dep.artifact_id.clone()));

            dep.version = dep
                .version
                .as_deref()
                .map(|v| resolve_placeholders(v, &properties))
                .or_else(|| {
                    managed
                        .and_then(|managed| managed.version.as_deref())
                        .map(|v| resolve_placeholders(v, &properties))
                });

            // dependencyManagement provides defaults for other fields when absent.
            if dep.scope.is_none() {
                dep.scope = managed
                    .and_then(|managed| managed.scope.as_deref())
                    .map(|v| resolve_placeholders(v, &properties));
            }
            if dep.classifier.is_none() {
                dep.classifier = managed
                    .and_then(|managed| managed.classifier.as_deref())
                    .map(|v| resolve_placeholders(v, &properties));
            }
            if dep.type_.is_none() {
                dep.type_ = managed
                    .and_then(|managed| managed.type_.as_deref())
                    .map(|v| resolve_placeholders(v, &properties));
            }

            if let Some(managed) = managed {
                if !dep.optional_specified {
                    dep.optional = managed.optional;
                    dep.optional_specified = managed.optional_specified;
                }
                dep.exclusions.extend(managed.exclusions.iter().filter_map(
                    |(group_id, artifact_id)| {
                        let group_id = resolve_placeholders(group_id, &properties);
                        let artifact_id = resolve_placeholders(artifact_id, &properties);
                        if group_id.is_empty() || artifact_id.is_empty() {
                            None
                        } else {
                            Some((group_id, artifact_id))
                        }
                    },
                ));
                normalize_exclusions(&mut dep.exclusions);
            }
            dependencies.push(dep);
        }

        Self {
            group_id,
            artifact_id,
            version,
            java,
            properties,
            compiler_plugin,
            dependency_management,
            dependencies,
        }
    }
}

impl RawPom {
    fn with_active_profiles_applied(&self) -> RawPom {
        let mut merged = self.clone();
        for profile in &self.profiles {
            if !profile.active_by_default {
                continue;
            }
            merged.properties.extend(profile.properties.clone());
            merged.dependencies.extend(profile.dependencies.clone());
            merged
                .dependency_management
                .extend(profile.dependency_management.clone());
        }
        merged
    }
}

fn is_bom_import(dep: &PomDependency) -> bool {
    dep.type_.as_deref() == Some("pom") && dep.scope.as_deref() == Some("import")
}

#[derive(Debug)]
struct MavenResolver {
    maven_repo: PathBuf,
    raw_cache: HashMap<PathBuf, RawPom>,
    effective_cache: HashMap<PathBuf, Arc<EffectivePom>>,
}

impl MavenResolver {
    fn new(maven_repo: PathBuf) -> Self {
        Self {
            maven_repo,
            raw_cache: HashMap::new(),
            effective_cache: HashMap::new(),
        }
    }

    fn cache_raw_pom(&mut self, pom_path: &Path, raw: RawPom) {
        let pom_path = canonicalize_or_fallback(pom_path);
        self.raw_cache.insert(pom_path, raw);
    }

    fn effective_pom_from_path(&mut self, pom_path: &Path) -> Option<Arc<EffectivePom>> {
        let mut visiting = HashSet::new();
        self.effective_pom_from_path_inner(pom_path, &mut visiting)
    }

    fn effective_pom_from_gav(
        &mut self,
        group_id: &str,
        artifact_id: &str,
        version: &str,
    ) -> Option<Arc<EffectivePom>> {
        let mut visiting = HashSet::new();
        self.effective_pom_from_gav_inner(group_id, artifact_id, version, &mut visiting)
    }

    fn effective_pom_from_gav_inner(
        &mut self,
        group_id: &str,
        artifact_id: &str,
        version: &str,
        visiting: &mut HashSet<PathBuf>,
    ) -> Option<Arc<EffectivePom>> {
        if group_id.is_empty() || artifact_id.is_empty() || version.is_empty() {
            return None;
        }
        if group_id.contains("${") || artifact_id.contains("${") || version.contains("${") {
            return None;
        }

        let pom_path = self.pom_path_in_repo(group_id, artifact_id, version);
        if !pom_path.is_file() {
            return None;
        }

        self.effective_pom_from_path_inner(&pom_path, visiting)
    }

    fn effective_pom_from_path_inner(
        &mut self,
        pom_path: &Path,
        visiting: &mut HashSet<PathBuf>,
    ) -> Option<Arc<EffectivePom>> {
        let pom_path = canonicalize_or_fallback(pom_path);

        if let Some(cached) = self.effective_cache.get(&pom_path) {
            return Some(Arc::clone(cached));
        }

        if !visiting.insert(pom_path.clone()) {
            return None;
        }

        let raw = match self.raw_cache.get(&pom_path).cloned() {
            Some(raw) => raw,
            None => {
                let raw = match parse_pom(&pom_path) {
                    Ok(raw) => raw,
                    Err(_) => {
                        visiting.remove(&pom_path);
                        return None;
                    }
                };
                self.raw_cache.insert(pom_path.clone(), raw.clone());
                raw
            }
        };

        let module_root = pom_path.parent().unwrap_or(Path::new("."));
        let parent = self.resolve_parent_effective(&raw, module_root, visiting);

        let effective = Arc::new(EffectivePom::from_raw(
            &raw,
            parent.as_deref(),
            self,
            visiting,
        ));
        self.effective_cache
            .insert(pom_path.clone(), Arc::clone(&effective));

        visiting.remove(&pom_path);
        Some(effective)
    }

    fn resolve_parent_effective(
        &mut self,
        raw: &RawPom,
        module_root: &Path,
        visiting: &mut HashSet<PathBuf>,
    ) -> Option<Arc<EffectivePom>> {
        let parent = raw.parent.as_ref()?;

        // 1) Explicit relativePath (if present and non-empty).
        if let Some(relative_path) = parent
            .relative_path
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            let candidate = module_root.join(relative_path);
            if candidate.is_file() {
                return self.effective_pom_from_path_inner(&candidate, visiting);
            }
        }

        // 2) Maven default: ../pom.xml.
        let default_candidate = module_root.join("../pom.xml");
        if default_candidate.is_file() {
            return self.effective_pom_from_path_inner(&default_candidate, visiting);
        }

        // 3) Local repository (best-effort).
        let group_id = parent.group_id.as_deref()?;
        let artifact_id = parent.artifact_id.as_deref()?;
        let version = parent.version.as_deref()?;
        self.effective_pom_from_gav_inner(group_id, artifact_id, version, visiting)
    }

    fn pom_path_in_repo(&self, group_id: &str, artifact_id: &str, version: &str) -> PathBuf {
        let group_path = group_id.replace('.', "/");
        self.maven_repo
            .join(group_path)
            .join(artifact_id)
            .join(version)
            .join(format!("{artifact_id}-{version}.pom"))
    }

    fn resolve_dependency_closure(&mut self, deps: &[PomDependency]) -> Vec<Dependency> {
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        struct DepKey {
            group_id: String,
            artifact_id: String,
            classifier: Option<String>,
            type_: Option<String>,
        }

        #[derive(Debug, Clone)]
        struct NodeState {
            /// Intersection of exclusions across all discovered paths to this node.
            exclusions: BTreeSet<(String, String)>,
            /// Exclusions used for the last expansion of this node, if any.
            expanded_with: Option<BTreeSet<(String, String)>>,
            /// A representative resolved version (if known) for loading the dependency's POM.
            version: Option<String>,
        }

        #[derive(Debug, Clone)]
        struct QueueItem {
            dep: PomDependency,
            exclusions: BTreeSet<(String, String)>,
        }

        let mut out = Vec::new();
        let mut queue: VecDeque<QueueItem> = deps
            .iter()
            .cloned()
            .map(|dep| QueueItem {
                exclusions: dep.exclusions.clone(),
                dep,
            })
            .collect();

        let mut seen: HashSet<DepKey> = HashSet::new();
        let mut nodes: HashMap<DepKey, NodeState> = HashMap::new();

        while let Some(item) = queue.pop_front() {
            let dep = item.dep;
            if dep.group_id.is_empty() || dep.artifact_id.is_empty() {
                continue;
            }

            let key = DepKey {
                group_id: dep.group_id.clone(),
                artifact_id: dep.artifact_id.clone(),
                classifier: dep.classifier.clone(),
                type_: dep.type_.clone(),
            };

            if seen.insert(key.clone()) {
                out.push(dep.as_public());
            }

            // Update node state (exclusions + best-known version), and decide whether to expand.
            let (exclusions, version, should_expand) = {
                let state = nodes.entry(key).or_insert_with(|| NodeState {
                    exclusions: item.exclusions.clone(),
                    expanded_with: None,
                    version: None,
                });

                let new_intersection = exclusion_intersection(&state.exclusions, &item.exclusions);
                if new_intersection != state.exclusions {
                    state.exclusions = new_intersection;
                }

                if state.version.is_none() {
                    if let Some(v) = dep
                        .version
                        .as_deref()
                        .map(str::trim)
                        .filter(|v| !v.is_empty())
                    {
                        if !v.contains("${") {
                            state.version = Some(v.to_string());
                        }
                    }
                }

                let should_expand = state.version.is_some()
                    && state.expanded_with.as_ref() != Some(&state.exclusions);
                if should_expand {
                    state.expanded_with = Some(state.exclusions.clone());
                }

                (
                    state.exclusions.clone(),
                    state.version.clone(),
                    should_expand,
                )
            };

            if !should_expand {
                continue;
            }

            let Some(version) = version else {
                continue;
            };

            let Some(effective) =
                self.effective_pom_from_gav(&dep.group_id, &dep.artifact_id, &version)
            else {
                continue;
            };

            for child in &effective.dependencies {
                if child.group_id.is_empty() || child.artifact_id.is_empty() {
                    continue;
                }

                // Optional dependencies are not transitively inherited.
                if child.optional {
                    continue;
                }

                // Exclusions apply transitively to this subtree.
                if exclusion_matches(&exclusions, &child.group_id, &child.artifact_id) {
                    continue;
                }

                let mut child_exclusions = exclusions.clone();
                child_exclusions.extend(child.exclusions.iter().cloned());
                normalize_exclusions(&mut child_exclusions);
                queue.push_back(QueueItem {
                    dep: child.clone(),
                    exclusions: child_exclusions,
                });
            }
        }

        out
    }
}

#[derive(Debug, Clone)]
struct WorkspaceModuleInfo {
    root: PathBuf,
    version: Option<String>,
    dependencies: Vec<PomDependency>,
}

type WorkspaceModuleIndex = HashMap<(String, String), WorkspaceModuleInfo>;

fn build_workspace_module_index(
    workspace_root: &Path,
    include_root_module: bool,
    modules: &[DiscoveredModule],
) -> WorkspaceModuleIndex {
    let mut out = WorkspaceModuleIndex::new();
    for module in modules {
        if module.root == workspace_root && !include_root_module {
            continue;
        }

        let group_id = module.effective.group_id.clone().unwrap_or_default();
        let artifact_id = module.effective.artifact_id.clone().unwrap_or_default();
        if group_id.is_empty() || artifact_id.is_empty() {
            continue;
        }

        out.insert(
            (group_id, artifact_id),
            WorkspaceModuleInfo {
                root: module.root.clone(),
                version: module.effective.version.clone(),
                dependencies: module.effective.dependencies.clone(),
            },
        );
    }
    out
}

fn is_workspace_module_dependency(dep: &Dependency, modules: &WorkspaceModuleIndex) -> bool {
    modules
        .get(&(dep.group_id.clone(), dep.artifact_id.clone()))
        .is_some_and(|m| versions_compatible(dep.version.as_deref(), m.version.as_deref()))
}

fn versions_compatible(requested: Option<&str>, available: Option<&str>) -> bool {
    let Some(requested) = requested.filter(|v| !v.trim().is_empty()) else {
        return true;
    };
    if requested.contains("${") {
        // Best-effort: if we couldn't resolve the version, treat it as compatible.
        return true;
    }

    let Some(available) = available.filter(|v| !v.trim().is_empty()) else {
        return true;
    };
    requested == available
}

fn parse_pom(path: &Path) -> Result<RawPom, ProjectError> {
    let contents = std::fs::read_to_string(path).map_err(|source| ProjectError::Io {
        path: path.to_path_buf(),
        source,
    })?;

    let doc = roxmltree::Document::parse(&contents).map_err(|source| ProjectError::Xml {
        path: path.to_path_buf(),
        source,
    })?;

    let project = doc.root_element();

    let mut pom = RawPom::default();
    pom.group_id = child_text(&project, "groupId");
    pom.artifact_id = child_text(&project, "artifactId");
    pom.version = child_text(&project, "version");
    pom.packaging = child_text(&project, "packaging");

    if let Some(parent_node) = child_element(&project, "parent") {
        pom.parent = Some(PomParent {
            group_id: child_text(&parent_node, "groupId"),
            artifact_id: child_text(&parent_node, "artifactId"),
            version: child_text(&parent_node, "version"),
            relative_path: child_text(&parent_node, "relativePath"),
        });
    }

    if let Some(props_node) = child_element(&project, "properties") {
        pom.properties = parse_properties(&props_node);
    }

    pom.compiler_plugin = parse_maven_compiler_plugin_config(&project);

    // dependencies
    if let Some(deps_node) = child_element(&project, "dependencies") {
        pom.dependencies = parse_dependencies(&deps_node);
    }

    if let Some(dep_mgmt) = child_element(&project, "dependencyManagement") {
        if let Some(deps_node) = child_element(&dep_mgmt, "dependencies") {
            pom.dependency_management = parse_dependencies(&deps_node);
        }
    }

    // modules
    if let Some(modules_node) = child_element(&project, "modules") {
        pom.modules.extend(parse_modules_list(&modules_node));
    }

    // profile modules (activeByDefault only)
    if let Some(profiles_node) = child_element(&project, "profiles") {
        for profile_node in profiles_node
            .children()
            .filter(|n| n.is_element() && n.has_tag_name("profile"))
        {
            let active_by_default = child_element(&profile_node, "activation")
                .and_then(|activation| child_text(&activation, "activeByDefault"))
                .is_some_and(|v| v.eq_ignore_ascii_case("true"));
            if !active_by_default {
                continue;
            }

            if let Some(modules_node) = child_element(&profile_node, "modules") {
                pom.modules.extend(parse_modules_list(&modules_node));
            }
        }
    }

    // profiles (minimum viable: activeByDefault)
    if let Some(profiles_node) = child_element(&project, "profiles") {
        pom.profiles = profiles_node
            .children()
            .filter(|n| n.is_element() && n.has_tag_name("profile"))
            .map(parse_profile)
            .collect();
    }

    Ok(pom)
}

fn parse_properties(node: &roxmltree::Node<'_, '_>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for child in node.children().filter(|n| n.is_element()) {
        let key = child.tag_name().name().to_string();
        if let Some(value) = child.text().map(str::trim).filter(|t| !t.is_empty()) {
            out.insert(key, value.to_string());
        }
    }
    out
}

fn parse_profile(profile_node: roxmltree::Node<'_, '_>) -> RawProfile {
    let active_by_default = child_element(&profile_node, "activation")
        .and_then(|activation| child_text(&activation, "activeByDefault"))
        .is_some_and(|t| t.eq_ignore_ascii_case("true"));

    let properties = child_element(&profile_node, "properties")
        .map(|n| parse_properties(&n))
        .unwrap_or_default();

    let dependencies = child_element(&profile_node, "dependencies")
        .map(|n| parse_dependencies(&n))
        .unwrap_or_default();

    let dependency_management =
        if let Some(dep_mgmt) = child_element(&profile_node, "dependencyManagement") {
            child_element(&dep_mgmt, "dependencies")
                .map(|deps| parse_dependencies(&deps))
                .unwrap_or_default()
        } else {
            Vec::new()
        };

    RawProfile {
        active_by_default,
        properties,
        dependencies,
        dependency_management,
    }
}
fn parse_modules_list(modules_node: &roxmltree::Node<'_, '_>) -> Vec<String> {
    modules_node
        .children()
        .filter(|n| n.is_element() && n.has_tag_name("module"))
        .filter_map(|n| n.text())
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

fn parse_dependencies(deps_node: &roxmltree::Node<'_, '_>) -> Vec<PomDependency> {
    deps_node
        .children()
        .filter(|n| n.is_element() && n.has_tag_name("dependency"))
        .filter_map(|dep_node| {
            let group_id = child_text(&dep_node, "groupId")?;
            let artifact_id = child_text(&dep_node, "artifactId")?;
            let version = child_text(&dep_node, "version");
            let scope = child_text(&dep_node, "scope");
            let classifier = child_text(&dep_node, "classifier");
            let type_ = child_text(&dep_node, "type");
            let optional_text = child_text(&dep_node, "optional");
            let optional_specified = optional_text.is_some();
            let optional = optional_text.is_some_and(|v| v.eq_ignore_ascii_case("true"));

            let mut exclusions = BTreeSet::new();
            if let Some(exclusions_node) = child_element(&dep_node, "exclusions") {
                for exclusion_node in exclusions_node
                    .children()
                    .filter(|n| n.is_element() && n.has_tag_name("exclusion"))
                {
                    let Some(group_id) = child_text(&exclusion_node, "groupId") else {
                        continue;
                    };
                    let Some(artifact_id) = child_text(&exclusion_node, "artifactId") else {
                        continue;
                    };
                    exclusions.insert((group_id, artifact_id));
                }
            }
            normalize_exclusions(&mut exclusions);

            Some(PomDependency {
                group_id,
                artifact_id,
                version,
                scope,
                classifier,
                type_,
                optional,
                optional_specified,
                exclusions,
            })
        })
        .collect()
}

fn pom_declares_java_config(pom: &RawPom) -> bool {
    pom.properties.contains_key("maven.compiler.release")
        || pom.properties.contains_key("maven.compiler.source")
        || pom.properties.contains_key("maven.compiler.target")
        || pom.compiler_plugin.is_some()
}

fn parse_maven_compiler_plugin_config(
    project: &roxmltree::Node<'_, '_>,
) -> Option<RawMavenCompilerPluginConfig> {
    let build = child_element(project, "build")?;

    // pluginManagement provides defaults; build/plugins overrides them.
    let mut config = RawMavenCompilerPluginConfig::default();
    let mut found_any = false;

    if let Some(plugin_mgmt) = child_element(&build, "pluginManagement") {
        if let Some(plugins) = child_element(&plugin_mgmt, "plugins") {
            if let Some(from_pm) = parse_maven_compiler_plugin_config_from_plugins(&plugins) {
                config.merge(from_pm);
                found_any = true;
            }
        }
    }

    if let Some(plugins) = child_element(&build, "plugins") {
        if let Some(from_plugins) = parse_maven_compiler_plugin_config_from_plugins(&plugins) {
            config.merge(from_plugins);
            found_any = true;
        }
    }

    if found_any && !config.is_empty() {
        Some(config)
    } else {
        None
    }
}

fn parse_maven_compiler_plugin_config_from_plugins(
    plugins: &roxmltree::Node<'_, '_>,
) -> Option<RawMavenCompilerPluginConfig> {
    let mut out = RawMavenCompilerPluginConfig::default();
    let mut found_any = false;

    for plugin in plugins
        .children()
        .filter(|n| n.is_element() && n.has_tag_name("plugin"))
    {
        let artifact_id = child_text(&plugin, "artifactId");
        if artifact_id.as_deref() != Some("maven-compiler-plugin") {
            continue;
        }

        let Some(configuration) = child_element(&plugin, "configuration") else {
            continue;
        };

        let mut cfg = RawMavenCompilerPluginConfig::default();
        cfg.release = child_text(&configuration, "release");
        cfg.source = child_text(&configuration, "source");
        cfg.target = child_text(&configuration, "target");

        if let Some(compiler_args) = child_element(&configuration, "compilerArgs") {
            for arg in compiler_args
                .children()
                .filter(|n| n.is_element() && n.has_tag_name("arg"))
            {
                if let Some(value) = arg.text().map(str::trim).filter(|t| !t.is_empty()) {
                    cfg.compiler_args.push(value.to_string());
                }
            }
        }

        if let Some(argument) = child_text(&configuration, "compilerArgument") {
            cfg.compiler_args
                .extend(argument.split_whitespace().map(|s| s.to_string()));
        }

        if !cfg.is_empty() {
            out.merge(cfg);
            found_any = true;
        }
    }

    if found_any && !out.is_empty() {
        Some(out)
    } else {
        None
    }
}

fn java_from_maven_config(
    props: &BTreeMap<String, String>,
    compiler_plugin: Option<&RawMavenCompilerPluginConfig>,
) -> Option<JavaConfig> {
    let enable_preview_from_props = |key: &str| {
        props.get(key).is_some_and(|raw| {
            let resolved = resolve_placeholders(raw, props);
            // Best-effort: treat the property value as a whitespace-separated list of args.
            // Maven projects also commonly set this as a single string that may contain other
            // flags, so we accept substring matches as well.
            resolved.split_whitespace().any(|arg| {
                arg.trim_matches(|c| matches!(c, '"' | '\'')).trim() == "--enable-preview"
            }) || resolved.contains("--enable-preview")
        })
    };

    let enable_preview = compiler_plugin.is_some_and(|cfg| cfg.enable_preview(props))
        || enable_preview_from_props("maven.compiler.compilerArgs")
        || enable_preview_from_props("maven.compiler.compilerArgument");

    let resolved_java_version = |key: &str| {
        props
            .get(key)
            .map(|raw| resolve_placeholders(raw, props))
            .filter(|resolved| !resolved.contains("${"))
            .and_then(|resolved| JavaVersion::parse(&resolved))
    };

    let release = resolved_java_version("maven.compiler.release");

    // `maven.compiler.release` property always wins over plugin config.
    if let Some(v) = release {
        return Some(JavaConfig {
            source: v,
            target: v,
            enable_preview,
        });
    }

    let plugin_release = compiler_plugin.and_then(|cfg| cfg.release(props));
    if let Some(v) = plugin_release {
        return Some(JavaConfig {
            source: v,
            target: v,
            enable_preview,
        });
    }

    let plugin_source = compiler_plugin.and_then(|cfg| cfg.source(props));
    let plugin_target = compiler_plugin.and_then(|cfg| cfg.target(props));
    if plugin_source.is_some() || plugin_target.is_some() {
        let source = plugin_source.or(plugin_target).expect("checked");
        let target = plugin_target.or(Some(source)).unwrap_or(source);
        return Some(JavaConfig {
            source,
            target,
            enable_preview,
        });
    }

    let source = resolved_java_version("maven.compiler.source");
    let target = resolved_java_version("maven.compiler.target");

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
        (None, None) => None,
    }
}

fn discover_maven_repo(workspace_root: &Path, options: &LoadOptions) -> PathBuf {
    options
        .maven_repo
        .clone()
        .or_else(|| maven_repo_from_maven_config(workspace_root))
        .or_else(maven_repo_from_user_settings)
        .or_else(default_maven_repo)
        .unwrap_or_else(|| PathBuf::from(".m2/repository"))
}

fn maven_repo_from_maven_config(workspace_root: &Path) -> Option<PathBuf> {
    let config_path = workspace_root.join(".mvn").join("maven.config");
    let contents = std::fs::read_to_string(&config_path).ok()?;

    // `.mvn/maven.config` uses whitespace-delimited JVM/maven command line arguments.
    // The `-Dmaven.repo.local` property can be expressed as:
    // - `-Dmaven.repo.local=/path/to/repo`
    // - `-Dmaven.repo.local /path/to/repo`
    //
    // We accept both, prefer the last valid value, and ignore placeholder values (e.g.
    // `${user.home}`) that we don't currently expand.
    let mut it = contents.split_whitespace().peekable();
    let mut repo: Option<PathBuf> = None;

    while let Some(raw_token) = it.next() {
        let token = raw_token.trim_matches(|c| matches!(c, '"' | '\''));
        if let Some(value) = token.strip_prefix("-Dmaven.repo.local=") {
            if let Some(path) = resolve_maven_repo_path(value, workspace_root) {
                repo = Some(path);
            }
            continue;
        }

        if token == "-Dmaven.repo.local" {
            if let Some(raw_value) = it.next() {
                let value = raw_value.trim_matches(|c| matches!(c, '"' | '\''));
                if let Some(path) = resolve_maven_repo_path(value, workspace_root) {
                    repo = Some(path);
                }
            }
            continue;
        }
    }

    repo
}

fn maven_repo_from_user_settings() -> Option<PathBuf> {
    let home = home_dir()?;
    let path = home.join(".m2").join("settings.xml");
    let contents = std::fs::read_to_string(&path).ok()?;

    let doc = roxmltree::Document::parse(&contents).ok()?;
    let local_repo = doc
        .descendants()
        .find(|node| node.is_element() && node.tag_name().name() == "localRepository")
        .and_then(|node| node.text())
        .map(str::trim)
        .filter(|text| !text.is_empty())?;

    resolve_maven_repo_path(local_repo, &home)
}

fn resolve_maven_repo_path(value: &str, base: &Path) -> Option<PathBuf> {
    let value = value
        .trim()
        .trim_matches(|c| matches!(c, '"' | '\''))
        .trim();
    if value.is_empty() {
        return None;
    }

    // Best-effort: don't try to resolve placeholders in Maven repo configuration.
    if value.contains("${") {
        return None;
    }

    let path = PathBuf::from(value);
    if path.is_absolute() {
        Some(path)
    } else {
        Some(base.join(path))
    }
}

fn default_maven_repo() -> Option<PathBuf> {
    Some(home_dir()?.join(".m2/repository"))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn exists_as_jar(path: &Path) -> bool {
    // Maven dependency artifacts are typically `.jar` files, but some build systems (and test
    // fixtures) may "explode" jars into directories (often still ending with `.jar`).
    path.is_file() || path.is_dir()
}
fn maven_dependency_jar_path(maven_repo: &Path, dep: &Dependency) -> Option<PathBuf> {
    let version = dep.version.as_deref()?;
    if version.contains("${") {
        return None;
    }

    if dep.group_id.contains("${") || dep.artifact_id.contains("${") {
        return None;
    }

    let type_ = dep.type_.as_deref().unwrap_or("jar");
    if type_ != "jar" {
        return None;
    }

    let classifier = dep.classifier.as_deref();
    if classifier.is_some_and(|c| c.contains("${")) {
        return None;
    }

    let group_path = dep.group_id.replace('.', "/");
    let version_dir = maven_repo
        .join(group_path)
        .join(&dep.artifact_id)
        .join(version);

    let default_file_name = |version: &str| {
        if let Some(classifier) = classifier {
            format!("{}-{}-{}.jar", dep.artifact_id, version, classifier)
        } else {
            format!("{}-{}.jar", dep.artifact_id, version)
        }
    };

    if version.ends_with("-SNAPSHOT") {
        // Prefer using Maven metadata to resolve the timestamped SNAPSHOT jar filename.
        if let Some(resolved) =
            resolve_snapshot_jar_file_name(&version_dir, &dep.artifact_id, classifier)
        {
            let resolved_path = version_dir.join(resolved);
            if resolved_path.is_file() || resolved_path.is_dir() {
                return Some(resolved_path);
            }
        }

        // If the timestamped SNAPSHOT jar isn't present in the repo, fall back to the
        // conventional `<artifactId>-<version>(-classifier).jar` path, as some local repos (and
        // build tools) store snapshots without timestamped filenames.
        let fallback = version_dir.join(default_file_name(version));
        return exists_as_jar(&fallback).then_some(fallback);
    }

    let path = version_dir.join(default_file_name(version));
    exists_as_jar(&path).then_some(path)
}

fn exists_as_jar(path: &Path) -> bool {
    path.is_file()
}

fn resolve_snapshot_jar_file_name(
    version_dir: &Path,
    artifact_id: &str,
    classifier: Option<&str>,
) -> Option<String> {
    // Maven stores SNAPSHOT artifacts as timestamped versions in the local repo, e.g.
    // `dep-1.0-20260112.123456-1.jar`.
    //
    // Resolve the timestamped version from `maven-metadata(-local).xml` when present, and fall
    // back to the conventional `<artifactId>-<version>(-classifier).jar` filename otherwise.
    let mut best_value: Option<String> = None;

    for metadata_name in ["maven-metadata-local.xml", "maven-metadata.xml"] {
        let metadata_path = version_dir.join(metadata_name);
        let Ok(contents) = std::fs::read_to_string(&metadata_path) else {
            continue;
        };
        let Ok(doc) = roxmltree::Document::parse(&contents) else {
            continue;
        };

        for node in doc
            .descendants()
            .filter(|n| n.is_element() && n.tag_name().name() == "snapshotVersion")
        {
            let ext = child_text(&node, "extension");
            if !ext.is_some_and(|e| e.eq_ignore_ascii_case("jar")) {
                continue;
            }

            let node_classifier = child_text(&node, "classifier");
            let classifier_matches = match (classifier, node_classifier.as_deref()) {
                (None, None) => true,
                (Some(wanted), Some(found)) => wanted == found,
                _ => false,
            };
            if !classifier_matches {
                continue;
            }

            let Some(value) = child_text(&node, "value") else {
                continue;
            };

            // Deterministic tie-breaker: pick the lexicographically max timestamped version.
            let replace = best_value.as_ref().is_none_or(|best| value > *best);
            if replace {
                best_value = Some(value);
            }
        }
    }

    let value = best_value?;
    Some(if let Some(classifier) = classifier {
        format!("{artifact_id}-{value}-{classifier}.jar")
    } else {
        format!("{artifact_id}-{value}.jar")
    })
}

fn child_element<'a>(
    node: &'a roxmltree::Node<'a, 'a>,
    name: &str,
) -> Option<roxmltree::Node<'a, 'a>> {
    node.children()
        .find(|n| n.is_element() && n.tag_name().name() == name)
}

fn child_text(node: &roxmltree::Node<'_, '_>, name: &str) -> Option<String> {
    child_element(node, name)
        .and_then(|n| n.text())
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

fn resolve_placeholders(text: &str, props: &BTreeMap<String, String>) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"\$\{([^}]+)\}").expect("valid regex"));

    // Maven properties can be nested, e.g. `${dep.version}` -> `${revision}` -> `1.2.3`.
    // Apply placeholder substitution repeatedly until the string stabilizes, or we hit a
    // small fixed iteration limit to avoid infinite loops/cycles.
    const MAX_ITERS: usize = 32;

    let mut current = text.to_string();
    let mut seen = Vec::new();

    for _ in 0..MAX_ITERS {
        if !current.contains("${") {
            break;
        }

        let next = re
            .replace_all(&current, |caps: &regex::Captures<'_>| {
                let key = &caps[1];
                props
                    .get(key)
                    .cloned()
                    .unwrap_or_else(|| caps[0].to_string())
            })
            .into_owned();

        if next == current {
            break;
        }

        // Break on cycles to avoid wasting iterations in oscillating substitutions.
        if seen.iter().any(|prev| prev == &next) {
            break;
        }

        seen.push(current);
        current = next;
    }

    current
}

fn push_source_root(
    out: &mut Vec<SourceRoot>,
    module_root: &Path,
    kind: SourceRootKind,
    rel: &str,
) -> bool {
    let path = module_root.join(rel);
    if path.is_dir() {
        out.push(SourceRoot {
            kind,
            origin: SourceRootOrigin::Source,
            path,
        });
        return true;
    }
    false
}

fn push_source_root_if_has_java(
    out: &mut Vec<SourceRoot>,
    module_root: &Path,
    kind: SourceRootKind,
    rel: &str,
) -> bool {
    let path = module_root.join(rel);
    if !path.is_dir() {
        return false;
    }

    let has_java = WalkDir::new(&path)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .any(|entry| {
            entry.file_type().is_file()
                && entry
                    .path()
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("java"))
        });

    if has_java {
        out.push(SourceRoot {
            kind,
            origin: SourceRootOrigin::Source,
            path,
        });
        return true;
    }

    false
}

fn sort_dedup_modules(modules: &mut Vec<Module>) {
    modules.sort_by(|a, b| a.root.cmp(&b.root).then(a.name.cmp(&b.name)));
    modules.dedup_by(|a, b| a.root == b.root);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn write_maven_config(workspace_root: &Path, contents: &str) {
        let mvn_dir = workspace_root.join(".mvn");
        std::fs::create_dir_all(&mvn_dir).expect("create .mvn");
        std::fs::write(mvn_dir.join("maven.config"), contents).expect("write maven.config");
    }

    #[test]
    fn parses_repo_local_equals_absolute_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_maven_config(dir.path(), "-Dmaven.repo.local=/abs/path");

        let repo = maven_repo_from_maven_config(dir.path()).expect("repo");
        assert_eq!(repo, PathBuf::from("/abs/path"));
    }

    #[test]
    fn parses_repo_local_space_separated_absolute_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_maven_config(dir.path(), "-Dmaven.repo.local /abs/path");

        let repo = maven_repo_from_maven_config(dir.path()).expect("repo");
        assert_eq!(repo, PathBuf::from("/abs/path"));
    }

    #[test]
    fn parses_repo_local_double_quoted_value() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_maven_config(dir.path(), r#"-Dmaven.repo.local="repo""#);

        let repo = maven_repo_from_maven_config(dir.path()).expect("repo");
        assert_eq!(repo, dir.path().join("repo"));
    }

    #[test]
    fn parses_repo_local_single_quoted_value() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_maven_config(dir.path(), "-Dmaven.repo.local='repo'");

        let repo = maven_repo_from_maven_config(dir.path()).expect("repo");
        assert_eq!(repo, dir.path().join("repo"));
    }

    #[test]
    fn relative_repo_local_resolves_to_workspace_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_maven_config(dir.path(), "-Dmaven.repo.local=repo");

        let repo = maven_repo_from_maven_config(dir.path()).expect("repo");
        assert_eq!(repo, dir.path().join("repo"));
    }

    #[test]
    fn placeholder_repo_local_is_ignored() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_maven_config(dir.path(), "-Dmaven.repo.local=${user.home}/.m2/repository");

        let repo = maven_repo_from_maven_config(dir.path());
        assert_eq!(repo, None);
    }

    #[test]
    fn maven_dependency_jar_path_omits_missing_jars() {
        let repo = tempfile::tempdir().expect("tempdir maven repo");

        let dep = Dependency {
            group_id: "com.example".to_string(),
            artifact_id: "dep".to_string(),
            version: Some("1.0".to_string()),
            scope: None,
            classifier: None,
            type_: None,
        };

        assert!(
            maven_dependency_jar_path(repo.path(), &dep).is_none(),
            "missing jars should be omitted from the classpath"
        );
    }

    #[test]
    fn maven_dependency_jar_path_accepts_snapshot_fallback_jar() {
        let repo = tempfile::tempdir().expect("tempdir maven repo");

        let jar_path = repo
            .path()
            .join("com/example/dep/1.0-SNAPSHOT/dep-1.0-SNAPSHOT.jar");
        if let Some(parent) = jar_path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir snapshot version dir");
        }
        std::fs::write(&jar_path, b"").expect("write jar placeholder");

        let dep = Dependency {
            group_id: "com.example".to_string(),
            artifact_id: "dep".to_string(),
            version: Some("1.0-SNAPSHOT".to_string()),
            scope: None,
            classifier: None,
            type_: None,
        };

        assert_eq!(
            maven_dependency_jar_path(repo.path(), &dep),
            Some(jar_path),
            "expected snapshot fallback jar to be accepted when present on disk"
        );
    }
}
