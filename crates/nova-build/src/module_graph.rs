use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

/// Identifier for a build-tool module within the workspace.
///
/// * Maven: relative path (`module-a`) or coordinates (`groupId:artifactId`)
/// * Gradle: project path (`:app`, `:lib:core`)
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
)]
pub struct ModuleId(String);

impl ModuleId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A directed module dependency graph (`from` depends on `to`).
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct ModuleGraph {
    pub edges: Vec<(ModuleId, ModuleId)>,
}

/// The minimal per-module information needed to infer a workspace module graph.
///
/// The graph inference is intentionally conservative: it only records edges
/// between *workspace* modules when one module's resolved compile classpath
/// contains another module's output directory, plus any explicit `project()`
/// dependencies that a build-tool integration can provide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleBuildInfo {
    pub id: ModuleId,
    /// The output directory for the module's main sources (e.g. `target/classes`).
    pub main_output_dir: PathBuf,
    /// The resolved compile classpath for the module.
    pub compile_classpath: Vec<PathBuf>,
    /// Optional explicit workspace-module dependencies (e.g. Gradle `project(":lib")`).
    pub project_dependencies: Vec<ModuleId>,
}

impl ModuleBuildInfo {
    pub fn new(
        id: ModuleId,
        main_output_dir: PathBuf,
        compile_classpath: Vec<PathBuf>,
    ) -> Self {
        Self {
            id,
            main_output_dir,
            compile_classpath,
            project_dependencies: Vec::new(),
        }
    }
}

/// Infer a module dependency graph from per-module compile classpaths and output dirs.
///
/// Edge direction: `from -> depends_on`.
///
/// The returned graph is deduplicated and deterministically ordered.
pub fn infer_module_graph(modules: &[ModuleBuildInfo]) -> ModuleGraph {
    let workspace_modules: HashSet<ModuleId> = modules.iter().map(|m| m.id.clone()).collect();

    // Map output dir path -> module ids that produce it. (Usually 1:1, but keep it general.)
    let mut output_dir_index: HashMap<PathBuf, Vec<ModuleId>> = HashMap::new();
    for module in modules {
        output_dir_index
            .entry(normalize_path(&module.main_output_dir))
            .or_default()
            .push(module.id.clone());
    }

    for ids in output_dir_index.values_mut() {
        ids.sort();
        ids.dedup();
    }

    let mut edges = Vec::new();

    for module in modules {
        // 1) Dependencies derived from resolved classpaths.
        for entry in &module.compile_classpath {
            let entry = normalize_path(entry);
            if let Some(targets) = output_dir_index.get(&entry) {
                for target in targets {
                    if target != &module.id {
                        edges.push((module.id.clone(), target.clone()));
                    }
                }
            }
        }

        // 2) Optional explicit workspace dependencies (Gradle `project()` deps).
        for dep in &module.project_dependencies {
            if dep == &module.id {
                continue;
            }
            if !workspace_modules.contains(dep) {
                continue;
            }
            edges.push((module.id.clone(), dep.clone()));
        }
    }

    edges.sort();
    edges.dedup();

    ModuleGraph { edges }
}

fn normalize_path(path: &Path) -> PathBuf {
    // `components()` removes redundant separators and trailing separators.
    // We also drop `.` components to avoid trivial mismatches when build tools
    // emit `.../target/./classes`.
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // Best-effort lexical normalization. Avoid popping past the root/prefix.
                if !out.pop() {
                    out.push(component.as_os_str());
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}
