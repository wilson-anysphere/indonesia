use std::collections::{BTreeMap, HashSet, VecDeque};
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

pub(crate) fn load_maven_project(
    root: &Path,
    options: &LoadOptions,
) -> Result<ProjectConfig, ProjectError> {
    let root_pom_path = root.join("pom.xml");
    let root_pom = parse_pom(&root_pom_path)?;
    let include_root_module =
        root_pom.packaging.as_deref() != Some("pom") || root_pom.modules.is_empty();

    let mut modules = Vec::new();
    let mut source_roots = Vec::new();
    let mut output_dirs = Vec::new();
    let mut dependencies = Vec::new();
    let mut classpath = Vec::new();
    let mut dependency_entries = Vec::new();

    let root_effective = Arc::new(EffectivePom::from_raw(&root_pom, None));
    let mut discovered_modules =
        discover_modules_recursive(root, &root_pom, Arc::clone(&root_effective))?;
    discovered_modules.sort_by(|a, b| a.root.cmp(&b.root));
    discovered_modules.dedup_by(|a, b| a.root == b.root);

    // Workspace-level Java config: take the maximum across modules so we don't
    // under-report language features used anywhere in the workspace.
    let mut workspace_java = root_effective.java.unwrap_or_default();

    let maven_repo = options
        .maven_repo
        .clone()
        .or_else(default_maven_repo)
        .unwrap_or_else(|| PathBuf::from(".m2/repository"));

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
        });

        // Maven standard source layout.
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

        // Dependencies.
        for dep in &effective.dependencies {
            if dep.group_id.is_empty() || dep.artifact_id.is_empty() {
                continue;
            }
            dependencies.push(dep.clone());

            if let Some(jar_path) = maven_dependency_jar_path(&maven_repo, &dep) {
                dependency_entries.push(ClasspathEntry {
                    kind: ClasspathEntryKind::Jar,
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

    let root_effective = Arc::new(EffectivePom::from_raw(&root_pom, None));
    let mut discovered_modules =
        discover_modules_recursive(root, &root_pom, Arc::clone(&root_effective))?;
    discovered_modules.sort_by(|a, b| a.root.cmp(&b.root));
    discovered_modules.dedup_by(|a, b| a.root == b.root);

    let maven_repo = options
        .maven_repo
        .clone()
        .or_else(default_maven_repo)
        .unwrap_or_else(|| PathBuf::from(".m2/repository"));

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
        let java_provenance = if module.raw_pom.java.is_some() {
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

        let mut dependencies = Vec::new();
        for dep in &effective.dependencies {
            if dep.group_id.is_empty() || dep.artifact_id.is_empty() {
                continue;
            }
            dependencies.push(dep.clone());

            if let Some(jar_path) = maven_dependency_jar_path(&maven_repo, dep) {
                classpath.push(ClasspathEntry {
                    kind: ClasspathEntryKind::Jar,
                    path: jar_path,
                });
            }
        }

        sort_dedup_source_roots(&mut source_roots);
        sort_dedup_output_dirs(&mut output_dirs);
        sort_dedup_classpath(&mut classpath);
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
        })
        .collect::<Vec<_>>();
    let jpms_modules = crate::jpms::discover_jpms_modules(&modules_for_jpms);

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
) -> Result<Vec<DiscoveredModule>, ProjectError> {
    let mut visited: HashSet<PathBuf> = HashSet::new();
    // `workspace_root` is canonicalized by `load_project_with_options`.
    visited.insert(workspace_root.to_path_buf());

    let mut out = vec![DiscoveredModule {
        root: workspace_root.to_path_buf(),
        raw_pom: root_pom.clone(),
        effective: Arc::clone(&root_effective),
    }];
    let mut queue: VecDeque<(PathBuf, Arc<EffectivePom>)> = VecDeque::new();

    let mut root_modules = root_pom.modules.clone();
    root_modules.sort();
    for module in root_modules {
        queue.push_back((workspace_root.join(module), Arc::clone(&root_effective)));
    }

    while let Some((module_root, parent_effective)) = queue.pop_front() {
        let module_root = canonicalize_or_fallback(&module_root);
        if !visited.insert(module_root.clone()) {
            continue;
        }

        let module_pom_path = module_root.join("pom.xml");
        let raw_pom = if module_pom_path.is_file() {
            parse_pom(&module_pom_path)?
        } else {
            RawPom::default()
        };

        let effective = Arc::new(EffectivePom::from_raw(
            &raw_pom,
            Some(parent_effective.as_ref()),
        ));

        let mut child_modules = raw_pom.modules.clone();
        child_modules.sort();
        for child in child_modules {
            queue.push_back((module_root.join(child), Arc::clone(&effective)));
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
    java: Option<JavaConfig>,
    dependencies: Vec<Dependency>,
    dependency_management: Vec<Dependency>,
    modules: Vec<String>,
    parent: Option<PomParent>,
}

#[derive(Debug, Clone)]
struct PomParent {
    group_id: Option<String>,
    version: Option<String>,
}

#[derive(Debug, Clone)]
struct EffectivePom {
    group_id: Option<String>,
    artifact_id: Option<String>,
    version: Option<String>,
    java: Option<JavaConfig>,
    properties: BTreeMap<String, String>,
    dependency_management: BTreeMap<(String, String), Dependency>,
    dependencies: Vec<Dependency>,
}

impl EffectivePom {
    fn from_raw(raw: &RawPom, parent: Option<&EffectivePom>) -> Self {
        let group_id = raw
            .group_id
            .clone()
            .or_else(|| raw.parent.as_ref().and_then(|p| p.group_id.clone()))
            .or_else(|| parent.and_then(|p| p.group_id.clone()));
        let artifact_id = raw
            .artifact_id
            .clone()
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

        // Resolve Java config after properties are merged.
        let java = raw
            .java
            .or_else(|| parent.and_then(|p| p.java))
            .or_else(|| java_from_properties(&properties));

        let mut dependency_management = parent
            .map(|p| p.dependency_management.clone())
            .unwrap_or_default();
        for dep in &raw.dependency_management {
            let mut dep = dep.clone();
            dep.version = dep
                .version
                .as_deref()
                .map(|v| resolve_placeholders(v, &properties));
            dependency_management.insert((dep.group_id.clone(), dep.artifact_id.clone()), dep);
        }

        let mut dependencies = Vec::new();
        for dep in &raw.dependencies {
            let mut dep = dep.clone();
            dep.version = dep
                .version
                .as_deref()
                .map(|v| resolve_placeholders(v, &properties))
                .or_else(|| {
                    dependency_management
                        .get(&(dep.group_id.clone(), dep.artifact_id.clone()))
                        .and_then(|managed| managed.version.clone())
                });
            dependencies.push(dep);
        }

        Self {
            group_id,
            artifact_id,
            version,
            java,
            properties,
            dependency_management,
            dependencies,
        }
    }
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
            version: child_text(&parent_node, "version"),
        });
    }

    if let Some(props_node) = child_element(&project, "properties") {
        for child in props_node.children().filter(|n| n.is_element()) {
            let key = child.tag_name().name().to_string();
            if let Some(value) = child.text().map(str::trim).filter(|t| !t.is_empty()) {
                pom.properties.insert(key, value.to_string());
            }
        }
    }

    pom.java = java_from_properties(&pom.properties);

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
        pom.modules = modules_node
            .children()
            .filter(|n| n.is_element() && n.has_tag_name("module"))
            .filter_map(|n| n.text())
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();
    }

    Ok(pom)
}

fn parse_dependencies(deps_node: &roxmltree::Node<'_, '_>) -> Vec<Dependency> {
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

            Some(Dependency {
                group_id,
                artifact_id,
                version,
                scope,
                classifier,
                type_,
            })
        })
        .collect()
}

fn java_from_properties(props: &BTreeMap<String, String>) -> Option<JavaConfig> {
    let release = props
        .get("maven.compiler.release")
        .and_then(|v| JavaVersion::parse(v));
    if let Some(v) = release {
        return Some(JavaConfig {
            source: v,
            target: v,
            enable_preview: false,
        });
    }

    let source = props
        .get("maven.compiler.source")
        .and_then(|v| JavaVersion::parse(v));
    let target = props
        .get("maven.compiler.target")
        .and_then(|v| JavaVersion::parse(v));

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

fn default_maven_repo() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)?;
    Some(home.join(".m2/repository"))
}

fn maven_dependency_jar_path(maven_repo: &Path, dep: &Dependency) -> Option<PathBuf> {
    let version = dep.version.as_deref()?;
    if version.contains("${") {
        return None;
    }

    let type_ = dep.type_.as_deref().unwrap_or("jar");
    if type_ != "jar" {
        return None;
    }

    let classifier = dep.classifier.as_deref();

    let group_path = dep.group_id.replace('.', "/");
    let base = maven_repo
        .join(group_path)
        .join(&dep.artifact_id)
        .join(version);

    let file_name = if let Some(classifier) = classifier {
        format!("{}-{}-{}.jar", dep.artifact_id, version, classifier)
    } else {
        format!("{}-{}.jar", dep.artifact_id, version)
    };

    Some(base.join(file_name))
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

    re.replace_all(text, |caps: &regex::Captures<'_>| {
        let key = &caps[1];
        props
            .get(key)
            .cloned()
            .unwrap_or_else(|| caps[0].to_string())
    })
    .into_owned()
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
