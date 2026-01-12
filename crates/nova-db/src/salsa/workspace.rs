use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nova_cache::normalize_rel_path;
use nova_classpath::{ClasspathEntry, ClasspathIndex, IndexOptions};
use nova_core::ClassId;
use nova_jdk::{JdkIndex, BUILTIN_JDK_BINARY_NAMES};
use nova_project::{
    BuildSystem, JavaConfig, JavaLanguageLevel, JpmsModuleRoot, JpmsWorkspace,
    LanguageLevelProvenance, Module, ModuleLanguageLevel, ProjectConfig, SourceRoot,
    SourceRootKind, SourceRootOrigin, WorkspaceModuleBuildId, WorkspaceModuleConfig,
    WorkspaceProjectModel,
};
use thiserror::Error;
use walkdir::WalkDir;

use crate::{FileId, ProjectId, SourceRootId};

use super::Database;
use super::NovaResolve;

/// Errors produced while loading a workspace and applying it to Salsa inputs.
#[derive(Debug, Error)]
pub enum WorkspaceLoadError {
    #[error(transparent)]
    Project(#[from] nova_project::ProjectError),
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ClasspathSpec {
    classpath: Vec<nova_project::ClasspathEntry>,
    module_path: Vec<nova_project::ClasspathEntry>,
    target_release: u16,
}

/// Glue between `nova-project`'s workspace model and `nova-db` Salsa inputs.
///
/// This struct owns the *stable id assignment* required by Salsa:
/// - `ProjectId`: stable per build module (Maven module / Gradle subproject / Bazel target set).
/// - `SourceRootId`: stable per (project, source root path).
///
/// It can be reused across reloads to preserve IDs and to avoid rebuilding expensive indexes when
/// the underlying inputs have not changed.
#[derive(Debug, Default)]
pub struct WorkspaceLoader {
    workspace_root: Option<PathBuf>,

    // Stable ids
    module_to_project: BTreeMap<String, ProjectId>,
    next_project_id: u32,

    // Projects active in the most recently loaded workspace model, in deterministic module-id
    // order. We keep this separate from `module_to_project` so we can preserve stable ids for
    // modules that disappear temporarily without treating them as part of the current workspace.
    active_projects: Vec<ProjectId>,

    source_root_ids: HashMap<(ProjectId, PathBuf), SourceRootId>,
    next_source_root_id: u32,

    // Stable type ids (host-managed `ClassId` allocator).
    class_ids: HashMap<(ProjectId, String), ClassId>,
    next_class_id: u32,

    // File ids are allocated by the host (typically a VFS); we cache the mapping so we can refer
    // back to files when they disappear.
    path_to_file: HashMap<PathBuf, FileId>,

    // Previous membership set (absolute paths) per project, for detecting deletions.
    project_file_paths: HashMap<ProjectId, HashSet<PathBuf>>,

    // Cached external indexes.
    jdk_index: Arc<JdkIndex>,
    classpath_specs: HashMap<ProjectId, ClasspathSpec>,
    classpath_indexes: HashMap<ProjectId, Option<Arc<ClasspathIndex>>>,
}

impl WorkspaceLoader {
    pub fn new() -> Self {
        // `JdkIndex::new()` is intentionally cheap and dependency-free. Consumers that need a real
        // JDK-backed index can build one externally and set it via `Database::set_jdk_index`.
        Self {
            jdk_index: Arc::new(JdkIndex::new()),
            ..Self::default()
        }
    }

    /// Returns the canonicalized workspace root this loader was last loaded for.
    pub fn workspace_root(&self) -> Option<&Path> {
        self.workspace_root.as_deref()
    }

    /// List all projects in the most recently loaded workspace model (stable order by module id).
    pub fn projects(&self) -> Vec<ProjectId> {
        self.active_projects.clone()
    }

    /// Look up the stable `ProjectId` for a build-module id (e.g. `maven:group:artifact`).
    pub fn project_id_for_module(&self, module_id: &str) -> Option<ProjectId> {
        self.module_to_project.get(module_id).copied()
    }

    /// Load (or reload) the workspace rooted at `path` and apply it to Salsa inputs.
    ///
    /// `file_id_for_path` must return stable `FileId`s for absolute file paths. Typically this is
    /// backed by `nova_vfs::Vfs::file_id`.
    pub fn load(
        &mut self,
        db: &Database,
        path: impl AsRef<Path>,
        file_id_for_path: &mut impl FnMut(&Path) -> FileId,
    ) -> Result<(), WorkspaceLoadError> {
        self.load_inner(db, path.as_ref(), None, file_id_for_path)
    }

    /// Reload the workspace and refresh the contents for any paths in `changed_files`.
    pub fn reload(
        &mut self,
        db: &Database,
        changed_files: &[PathBuf],
        file_id_for_path: &mut impl FnMut(&Path) -> FileId,
    ) -> Result<(), WorkspaceLoadError> {
        let workspace_root = self
            .workspace_root
            .clone()
            .unwrap_or_else(|| PathBuf::from("."));
        self.load_inner(db, &workspace_root, Some(changed_files), file_id_for_path)
    }

    fn load_inner(
        &mut self,
        db: &Database,
        path: &Path,
        changed_files: Option<&[PathBuf]>,
        file_id_for_path: &mut impl FnMut(&Path) -> FileId,
    ) -> Result<(), WorkspaceLoadError> {
        let model = match nova_project::load_workspace_model_with_workspace_config(path) {
            Ok(model) => model,
            Err(nova_project::ProjectError::UnknownProjectType { .. }) => {
                // Match `nova-workspace`'s historical behavior by treating "unknown" workspaces as a
                // simple single-module project rooted at `path`, using the root directory itself as
                // the source root. This allows empty folders (or folders without a `src/` yet) to
                // be opened in the IDE.
                fallback_workspace_model(path)
            }
            Err(err) => return Err(err.into()),
        };
        self.workspace_root = Some(model.workspace_root.clone());

        let changed_set = changed_files.map(|files| {
            files
                .iter()
                .map(|p| p.canonicalize().unwrap_or_else(|_| p.clone()))
                .collect::<HashSet<_>>()
        });

        // Keep module order deterministic independent of loader implementation details.
        let mut modules = model.modules.clone();
        modules.sort_by(|a, b| a.id.cmp(&b.id));

        let mut active_projects = Vec::with_capacity(modules.len());
        for module in modules {
            let project = self.project_id_for_module_or_insert(&module.id);
            active_projects.push(project);
            let project_config = project_config_from_workspace_module(&model, &module);
            let target_release = project_config.java.target.0;
            db.set_project_config(project, Arc::new(project_config));

            // External indexes.
            db.set_jdk_index(project, Arc::clone(&self.jdk_index));
            self.apply_classpath_index(db, project, &module, target_release);

            // File set + source roots.
            let file_roots = scan_java_files(&module.source_roots);
            let mut files_for_project = Vec::with_capacity(file_roots.len());
            let mut file_paths_set = HashSet::new();

            for (file_path, source_root_path) in file_roots {
                let is_new_file = !self.path_to_file.contains_key(&file_path);
                let file_id = self.file_id_for_path(file_path.as_path(), file_id_for_path);

                let rel = Arc::new(rel_path_under_root(&model.workspace_root, &file_path));
                db.set_file_rel_path(file_id, rel.clone());

                db.set_file_project(file_id, project);

                let source_root_id =
                    self.source_root_id_for_path(project, source_root_path.as_path());
                db.set_source_root(file_id, source_root_id);

                // Refresh file existence + content.
                db.set_file_exists(file_id, true);

                let should_refresh = changed_set
                    .as_ref()
                    .map(|set| set.contains(&file_path))
                    .unwrap_or(true)
                    || is_new_file;
                if should_refresh {
                    let text = std::fs::read_to_string(&file_path).map_err(|source| {
                        WorkspaceLoadError::Io {
                            path: file_path.clone(),
                            source,
                        }
                    })?;
                    db.set_file_content(file_id, Arc::new(text));
                }

                files_for_project.push((rel, file_id));
                file_paths_set.insert(file_path);
            }

            // Mark files that disappeared from the project as deleted without reusing ids.
            if let Some(prev) = self.project_file_paths.get(&project) {
                for path in prev.difference(&file_paths_set) {
                    if let Some(&file_id) = self.path_to_file.get(path) {
                        db.set_file_exists(file_id, false);
                    }
                }
            }
            self.project_file_paths.insert(project, file_paths_set);

            // Stable ordering for determinism: sort by `file_rel_path`.
            files_for_project.sort_by(|(a_path, a_id), (b_path, b_id)| {
                a_path
                    .cmp(b_path)
                    .then_with(|| a_id.to_raw().cmp(&b_id.to_raw()))
            });
            let files_for_project = files_for_project
                .into_iter()
                .map(|(_path, id)| id)
                .collect::<Vec<_>>();
            let files_for_project = Arc::new(files_for_project);
            db.set_project_files(project, files_for_project.clone());

            // Update the host-managed `ClassId` registry for all source types in this project.
            self.apply_project_class_ids(db, project, &files_for_project);
        }

        self.active_projects = active_projects;

        Ok(())
    }

    fn project_id_for_module_or_insert(&mut self, module_id: &str) -> ProjectId {
        if let Some(&id) = self.module_to_project.get(module_id) {
            return id;
        }
        let id = ProjectId::from_raw(self.next_project_id);
        self.next_project_id = self.next_project_id.saturating_add(1);
        self.module_to_project.insert(module_id.to_string(), id);
        id
    }

    pub fn source_root_id_for_path(&mut self, project: ProjectId, path: &Path) -> SourceRootId {
        let key = (project, path.to_path_buf());
        if let Some(&id) = self.source_root_ids.get(&key) {
            return id;
        }
        let id = SourceRootId::from_raw(self.next_source_root_id);
        self.next_source_root_id = self.next_source_root_id.saturating_add(1);
        self.source_root_ids.insert(key, id);
        id
    }

    fn file_id_for_path(
        &mut self,
        path: &Path,
        file_id_for_path: &mut impl FnMut(&Path) -> FileId,
    ) -> FileId {
        if let Some(&id) = self.path_to_file.get(path) {
            return id;
        }
        let id = file_id_for_path(path);
        self.path_to_file.insert(path.to_path_buf(), id);
        id
    }

    fn apply_classpath_index(
        &mut self,
        db: &Database,
        project: ProjectId,
        module: &WorkspaceModuleConfig,
        target_release: u16,
    ) {
        let spec = ClasspathSpec {
            classpath: module.classpath.clone(),
            module_path: module.module_path.clone(),
            target_release,
        };

        let unchanged = self
            .classpath_specs
            .get(&project)
            .is_some_and(|prev| prev == &spec);

        if unchanged {
            if let Some(index) = self.classpath_indexes.get(&project).cloned().flatten() {
                db.set_classpath_index(project, Some(index));
            } else {
                db.set_classpath_index(project, None);
            }
            return;
        }

        self.classpath_specs.insert(project, spec);

        let entries = module
            .classpath
            .iter()
            .chain(module.module_path.iter())
            .filter_map(|entry| classpath_entry_for_project_entry(entry))
            .collect::<Vec<_>>();

        // If persistence is enabled, use the per-project classpath cache directory for class-dir
        // indexing (jar/jmod entries are cached separately in the global deps cache).
        let classpath_cache_dir = db
            .inner
            .lock()
            .persistence
            .cache_dir()
            .map(|dir| dir.classpath_dir());

        let index = if entries.is_empty() {
            None
        } else {
            match ClasspathIndex::build_with_options(
                &entries,
                classpath_cache_dir.as_deref(),
                IndexOptions {
                    target_release: Some(target_release),
                },
            ) {
                Ok(index) => Some(Arc::new(index)),
                Err(_) => None,
            }
        };

        db.set_classpath_index(project, index.clone());
        self.classpath_indexes.insert(project, index);
    }

    fn apply_project_class_ids(&mut self, db: &Database, project: ProjectId, files: &[FileId]) {
        // Collect type binary names deterministically:
        // - Files are already provided in stable order (sorted by `file_rel_path`).
        // - Within each file, sort type names lexicographically.
        //
        // Additionally, include external classpath types from the already-built `ClasspathIndex`
        // (if any) for this project.
        //
        // NOTE(JPMS): `NovaInputs::classpath_index` is a legacy, non-module-aware index that may
        // contain both `--class-path` and `--module-path` entries (see `apply_classpath_index`).
        // The `project_class_ids` registry is an *identity map* and does not enforce module
        // readability/exports, so we include all names present in the index.
        let source_names: Vec<String> = db.with_snapshot(|snap| {
            let mut names = Vec::new();

            for &file in files {
                let map = snap.def_map(file);
                let mut file_names: Vec<String> = map
                    .iter_type_defs()
                    .map(|(_, def)| def.binary_name.as_str().to_string())
                    .collect();
                file_names.sort();
                file_names.dedup();
                names.extend(file_names);
            }

            names
        });

        let classpath_names: Vec<String> = self
            .classpath_indexes
            .get(&project)
            .and_then(|index| index.as_deref())
            .map(|index| {
                index
                    .iter_binary_names()
                    .filter(|name| !name.starts_with("java."))
                    .map(|name| name.to_owned())
                    .collect()
            })
            .unwrap_or_default();

        // Optional: seed a small, stable set of core JDK binary names (shared with the built-in
        // `nova-jdk` index) so semantic consumers can always obtain a `ClassId` for common JDK
        // types without indexing a full on-disk JDK.
        //
        // We intentionally avoid enumerating all JDK classes here: real JDKs can contain tens of
        // thousands of types, and the host-managed registry is monotonic across the process
        // lifetime.
        for name in source_names
            .into_iter()
            .chain(classpath_names)
            .chain(BUILTIN_JDK_BINARY_NAMES.iter().copied().map(String::from))
        {
            let key = (project, name);
            if self.class_ids.contains_key(&key) {
                continue;
            }

            let id = ClassId::from_raw(self.next_class_id);
            self.next_class_id = self.next_class_id.saturating_add(1);
            self.class_ids.insert(key, id);
        }

        // Provide a stable per-project mapping input to Salsa.
        let mut mapping: Vec<(Arc<str>, ClassId)> = self
            .class_ids
            .iter()
            .filter_map(|((proj, name), &id)| {
                (*proj == project).then(|| (Arc::<str>::from(name.as_str()), id))
            })
            .collect();
        mapping.sort_by(|(a_name, a_id), (b_name, b_id)| {
            a_name
                .as_ref()
                .cmp(b_name.as_ref())
                .then_with(|| a_id.to_raw().cmp(&b_id.to_raw()))
        });

        db.set_project_class_ids(project, Arc::new(mapping));
    }
}

fn fallback_workspace_model(root: &Path) -> WorkspaceProjectModel {
    let workspace_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let module_name = workspace_root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("root")
        .to_string();

    let source_roots = vec![SourceRoot {
        kind: SourceRootKind::Main,
        origin: SourceRootOrigin::Source,
        path: workspace_root.clone(),
    }];

    let module_config = WorkspaceModuleConfig {
        id: format!("simple:{module_name}"),
        name: module_name.clone(),
        root: workspace_root.clone(),
        build_id: WorkspaceModuleBuildId::Simple,
        language_level: ModuleLanguageLevel {
            level: JavaLanguageLevel::from_java_config(JavaConfig::default()),
            provenance: LanguageLevelProvenance::Default,
        },
        source_roots,
        output_dirs: Vec::new(),
        module_path: Vec::new(),
        classpath: Vec::new(),
        dependencies: Vec::new(),
    };

    WorkspaceProjectModel::new(
        workspace_root,
        BuildSystem::Simple,
        JavaConfig::default(),
        vec![module_config],
        Vec::new(),
    )
}

fn rel_path_under_root(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    normalize_rel_path(&rel.to_string_lossy())
}

fn scan_java_files(source_roots: &[SourceRoot]) -> Vec<(PathBuf, PathBuf)> {
    let mut out = Vec::new();
    let mut seen = HashMap::<PathBuf, (usize, PathBuf)>::new();

    for root in source_roots {
        let dir = &root.path;
        if !dir.is_dir() {
            continue;
        }

        let root_components = dir.components().count();
        for entry in WalkDir::new(dir)
            .follow_links(true)
            .into_iter()
            .filter_map(Result::ok)
        {
            if !entry.file_type().is_file() {
                continue;
            }
            if entry.path().extension().and_then(|e| e.to_str()) != Some("java") {
                continue;
            }

            let path = entry.into_path();
            match seen.get(&path) {
                None => {
                    seen.insert(path, (root_components, dir.clone()));
                }
                Some((prev_components, _)) if *prev_components < root_components => {
                    seen.insert(path, (root_components, dir.clone()));
                }
                _ => {}
            }
        }
    }

    for (path, (_depth, root)) in seen {
        out.push((path, root));
    }
    out.sort_by(|(a, _), (b, _)| a.cmp(b));
    out
}

fn classpath_entry_for_project_entry(
    entry: &nova_project::ClasspathEntry,
) -> Option<ClasspathEntry> {
    let converted = ClasspathEntry::from(entry);
    match &converted {
        ClasspathEntry::Jar(path) | ClasspathEntry::Jmod(path) => {
            path.exists().then_some(converted)
        }
        ClasspathEntry::ClassDir(_) => Some(converted),
    }
}

fn project_config_from_workspace_module(
    workspace: &WorkspaceProjectModel,
    module: &WorkspaceModuleConfig,
) -> ProjectConfig {
    let java = java_config_from_language_level(&module.language_level.level, workspace.java);

    let modules = vec![Module {
        name: module.name.clone(),
        root: module.root.clone(),
        annotation_processing: Default::default(),
    }];

    let mut jpms_modules: Vec<JpmsModuleRoot> = workspace
        .jpms_modules
        .iter()
        .filter(|m| m.root.starts_with(&module.root))
        .cloned()
        .collect();
    jpms_modules.sort_by(|a, b| a.name.cmp(&b.name));

    // Build a JPMS module graph that also includes named/automatic modules derived from
    // module-path entries. This matches legacy `load_project` behavior.
    let jpms_workspace =
        nova_project::jpms::build_jpms_workspace(&jpms_modules, &module.module_path);

    ProjectConfig {
        workspace_root: workspace.workspace_root.clone(),
        build_system: workspace.build_system,
        java,
        modules,
        jpms_modules,
        jpms_workspace,
        source_roots: module.source_roots.clone(),
        module_path: module.module_path.clone(),
        classpath: module.classpath.clone(),
        output_dirs: module.output_dirs.clone(),
        dependencies: module.dependencies.clone(),
        workspace_model: None,
    }
}

fn java_config_from_language_level(level: &JavaLanguageLevel, fallback: JavaConfig) -> JavaConfig {
    let source = level.release.or(level.source).unwrap_or(fallback.source);
    let target = level
        .release
        .or(level.target)
        .or(level.source)
        .unwrap_or(fallback.target);

    JavaConfig {
        source,
        target,
        enable_preview: level.preview || fallback.enable_preview,
    }
}
