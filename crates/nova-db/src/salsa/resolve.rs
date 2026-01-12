use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Instant;

use nova_core::{ClassId, Name, TypeName};
use nova_types::Diagnostic;

use crate::persistence::HasPersistence;
use crate::{FileId, ProjectId};

use super::cancellation as cancel;
use super::hir::NovaHir;
use super::jpms::{jpms_enabled, module_for_file, JpmsProjectIndex};
use super::stats::HasQueryStats;
use super::{
    ArcEq, InternedClassKey, InternedClassKeyId, NovaInternedClassKeys, TrackedSalsaMemo,
    TrackedSalsaProjectMemo,
};
use ra_salsa::InternKey;

#[ra_salsa::query_group(NovaResolveStorage)]
pub trait NovaResolve: NovaHir + HasQueryStats + HasPersistence + NovaInternedClassKeys {
    /// Build the scope graph for a file.
    fn scope_graph(&self, file: FileId) -> Arc<nova_resolve::ItemTreeScopeBuildResult>;

    /// File-level definition map used for workspace-wide name resolution.
    fn def_map(&self, file: FileId) -> Arc<nova_resolve::DefMap>;

    /// Import declarations for a file lowered into an [`nova_resolve::ImportMap`].
    fn import_map(&self, file: FileId) -> Arc<nova_resolve::ImportMap>;

    /// Workspace-wide type namespace for a project.
    fn workspace_def_map(&self, project: ProjectId) -> Arc<nova_resolve::WorkspaceDefMap>;

    /// Deterministic, query-derived mapping from workspace (source) type keys to `ClassId`.
    ///
    /// The mapping is global across all projects discovered via `all_file_ids()` and
    /// is stable across query order and memo eviction.
    fn workspace_class_id_map(&self) -> Arc<WorkspaceClassIdMap>;

    /// Lookup the stable `ClassId` for a workspace (source) type.
    fn class_id_for_type(&self, project: ProjectId, name: TypeName) -> Option<ClassId>;

    /// Inverse lookup: map a `ClassId` back to its `(ProjectId, TypeName)` key.
    fn class_key(&self, id: ClassId) -> Option<(ProjectId, TypeName)>;

    /// Intern all workspace class keys for `project` in a deterministic order.
    ///
    /// Salsa `#[interned]` IDs are assigned monotonically in insertion order; by
    /// forcing a single sorted insertion point we ensure:
    /// - stable `ClassId` for existing types across incremental edits (adding
    ///   new types does not renumber old ones)
    /// - deterministic ID assignment for multiple new types added in a single
    ///   revision
    fn workspace_interned_class_keys(&self, project: ProjectId) -> Arc<Vec<InternedClassKeyId>>;

    /// Map a workspace type name to a stable numeric [`nova_ids::ClassId`].
    ///
    /// Returns `None` if `name` is not defined in the workspace.
    fn class_id_for_workspace_type(&self, project: ProjectId, name: TypeName) -> Option<ClassId>;

    /// JPMS compilation environment (module graph + module-aware classpath index).
    fn jpms_compilation_env(
        &self,
        project: ProjectId,
    ) -> Option<ArcEq<nova_resolve::jpms_env::JpmsCompilationEnvironment>>;

    /// Best-effort diagnostics for import declarations in `file`.
    fn import_diagnostics(&self, file: FileId) -> Arc<Vec<Diagnostic>>;

    /// Resolve `name` starting from `scope`.
    fn resolve_name(
        &self,
        file: FileId,
        scope: nova_resolve::ScopeId,
        name: Name,
    ) -> Option<nova_resolve::Resolution>;

    /// Like [`NovaResolve::resolve_name`], but returns a detailed resolution result that preserves
    /// unresolved and ambiguous outcomes.
    fn resolve_name_detailed(
        &self,
        file: FileId,
        scope: nova_resolve::ScopeId,
        name: Name,
    ) -> nova_resolve::NameResolution;
}

/// Deterministic mapping between workspace (source) classes and [`ClassId`]s.
///
/// IDs are allocated globally across all known projects by interning
/// [`InternedClassKey`] values through Salsa's `#[ra_salsa::interned]` table.
///
/// ### Determinism + stability properties
///
/// - IDs are assigned *monotonically* by Salsa the first time a key is interned.
/// - This query forces a single deterministic interning order by sorting all
///   `(ProjectId, binary_name)` keys lexicographically before calling
///   [`NovaInternedClassKeys::intern_class_key`].
/// - As a result, existing IDs do **not** change when new workspace types are
///   added later; new keys are assigned fresh IDs without renumbering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceClassIdMap {
    by_key: HashMap<(ProjectId, String), ClassId>,
    by_id: HashMap<ClassId, (ProjectId, String)>,
}

impl WorkspaceClassIdMap {
    #[must_use]
    pub fn class_id_for_type(&self, project: ProjectId, name: &TypeName) -> Option<ClassId> {
        self.by_key
            .get(&(project, name.as_str().to_owned()))
            .copied()
    }

    #[must_use]
    pub fn class_key(&self, id: ClassId) -> Option<(ProjectId, TypeName)> {
        let (project, name) = self.by_id.get(&id)?;
        Some((*project, TypeName::new(name.clone())))
    }
}

fn scope_graph(db: &dyn NovaResolve, file: FileId) -> Arc<nova_resolve::ItemTreeScopeBuildResult> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "scope_graph", ?file).entered();

    cancel::check_cancelled(db);

    // Touch the file text so edits invalidate `scope_graph` and force a re-run.
    //
    // Even though the scope graph is derived from structural HIR (`hir_item_tree`) and often
    // remains *equal* across body-only edits, we still want Salsa to re-execute the query so
    // early-cutoff can keep downstream queries (like `resolve_name`) memoized while observing
    // that an edit occurred.
    if db.file_exists(file) {
        let _ = db.file_content(file);
    }
    let tree = db.hir_item_tree(file);
    let built = nova_resolve::build_scopes_for_item_tree(file, &tree);

    let result = Arc::new(built);
    // NOTE: This is a best-effort estimate used for memory pressure heuristics.
    let declared_items = (tree.items.len()
        + tree.imports.len()
        + tree.classes.len()
        + tree.interfaces.len()
        + tree.enums.len()
        + tree.records.len()
        + tree.annotations.len()
        + tree.fields.len()
        + tree.methods.len()
        + tree.constructors.len()
        + tree.initializers.len()) as u64;
    let scope_count = 4u64 // universe + package + import + file
        .saturating_add(result.class_scopes.len() as u64)
        .saturating_add(result.method_scopes.len() as u64)
        .saturating_add(result.constructor_scopes.len() as u64)
        .saturating_add(result.initializer_scopes.len() as u64);
    let approx_bytes = scope_count
        .saturating_mul(256)
        .saturating_add(declared_items.saturating_mul(64));
    db.record_salsa_memo_bytes(file, TrackedSalsaMemo::ScopeGraph, approx_bytes);
    db.record_query_stat("scope_graph", start.elapsed());
    result
}

fn def_map(db: &dyn NovaResolve, file: FileId) -> Arc<nova_resolve::DefMap> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "def_map", ?file).entered();

    cancel::check_cancelled(db);

    let tree = db.hir_item_tree(file);
    let map = nova_resolve::DefMap::from_item_tree(file, &tree);
    let result = Arc::new(map);
    db.record_salsa_memo_bytes(file, TrackedSalsaMemo::DefMap, result.estimated_bytes());
    db.record_query_stat("def_map", start.elapsed());
    result
}

fn import_map(db: &dyn NovaResolve, file: FileId) -> Arc<nova_resolve::ImportMap> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "import_map", ?file).entered();

    cancel::check_cancelled(db);

    let tree = db.hir_item_tree(file);
    let map = nova_resolve::ImportMap::from_item_tree(&tree);
    let result = Arc::new(map);
    db.record_salsa_memo_bytes(file, TrackedSalsaMemo::ImportMap, result.estimated_bytes());
    db.record_query_stat("import_map", start.elapsed());
    result
}

fn workspace_def_map(
    db: &dyn NovaResolve,
    project: ProjectId,
) -> Arc<nova_resolve::WorkspaceDefMap> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "workspace_def_map", ?project).entered();

    cancel::check_cancelled(db);

    let cfg = db.project_config(project);
    let jpms_enabled = jpms_enabled(&cfg);
    let files = db.project_files(project);
    let mut out = nova_resolve::WorkspaceDefMap::default();

    for (i, &file) in files.iter().enumerate() {
        cancel::checkpoint_cancelled_every(db, i as u32, 32);
        let map = db.def_map(file);
        if jpms_enabled {
            let rel = db.file_rel_path(file);
            let module = module_for_file(&cfg, rel.as_str());
            out.extend_from_def_map_with_module(&map, module);
        } else {
            out.extend_from_def_map(&map);
        }
    }

    let result = Arc::new(out);
    db.record_salsa_project_memo_bytes(
        project,
        TrackedSalsaProjectMemo::WorkspaceDefMap,
        result.estimated_bytes(),
    );
    db.record_query_stat("workspace_def_map", start.elapsed());
    result
}

fn workspace_class_id_map(db: &dyn NovaResolve) -> Arc<WorkspaceClassIdMap> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "workspace_class_id_map").entered();

    cancel::check_cancelled(db);

    let files = db.all_file_ids();
    let mut projects = BTreeSet::<ProjectId>::new();
    for (i, &file) in files.iter().enumerate() {
        cancel::checkpoint_cancelled_every(db, i as u32, 256);
        projects.insert(db.file_project(file));
    }

    let mut keys: Vec<(ProjectId, String)> = Vec::new();
    for (i, &project) in projects.iter().enumerate() {
        cancel::checkpoint_cancelled_every(db, i as u32, 32);
        let workspace = db.workspace_def_map(project);
        keys.extend(
            workspace
                .all_type_names()
                .map(|name| (project, name.as_str().to_owned())),
        );
    }

    keys.sort_by(|(a_project, a_name), (b_project, b_name)| {
        a_project
            .to_raw()
            .cmp(&b_project.to_raw())
            .then_with(|| a_name.cmp(b_name))
    });

    let mut by_key = HashMap::with_capacity(keys.len());
    let mut by_id = HashMap::with_capacity(keys.len());
    for (idx, (project, binary_name)) in keys.into_iter().enumerate() {
        cancel::checkpoint_cancelled_every(db, idx as u32, 256);

        let interned = db.intern_class_key(InternedClassKey {
            project,
            binary_name: binary_name.clone(),
        });

        // Persist the interned raw id as Nova's canonical `ClassId`:
        //   InternedClassKeyId -> ra_salsa::InternId -> u32 -> nova_core::ClassId
        let raw: u32 = interned.as_intern_id().as_u32();
        let id = ClassId::from_raw(raw);

        by_key.insert((project, binary_name.clone()), id);
        by_id.insert(id, (project, binary_name));
    }

    let result = Arc::new(WorkspaceClassIdMap { by_key, by_id });
    db.record_query_stat("workspace_class_id_map", start.elapsed());
    result
}

fn class_id_for_type(db: &dyn NovaResolve, project: ProjectId, name: TypeName) -> Option<ClassId> {
    db.workspace_class_id_map()
        .class_id_for_type(project, &name)
}

fn class_key(db: &dyn NovaResolve, id: ClassId) -> Option<(ProjectId, TypeName)> {
    db.workspace_class_id_map().class_key(id)
}

fn workspace_interned_class_keys(
    db: &dyn NovaResolve,
    project: ProjectId,
) -> Arc<Vec<InternedClassKeyId>> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span =
        tracing::debug_span!("query", name = "workspace_interned_class_keys", ?project).entered();

    cancel::check_cancelled(db);

    // Ensure interned IDs are seeded in a deterministic *global* order (across
    // all projects) before we materialize the per-project list below.
    let _ = db.workspace_class_id_map();

    let workspace = db.workspace_def_map(project);

    // NOTE: `WorkspaceDefMap` is backed by hash maps; iteration order is
    // intentionally unspecified. `iter_type_names` yields names in a
    // deterministic order (sorted by `TypeName::as_str()`), which we rely on to
    // guarantee stable bulk-intern insertion.
    let mut keys = Vec::with_capacity(workspace.all_type_names().size_hint().0);
    for (i, name) in workspace.iter_type_names().enumerate() {
        cancel::checkpoint_cancelled_every(db, i as u32, 256);
        let key = InternedClassKey {
            project,
            binary_name: name.as_str().to_string(),
        };
        keys.push(db.intern_class_key(key));
    }

    let result = Arc::new(keys);
    db.record_query_stat("workspace_interned_class_keys", start.elapsed());
    result
}

fn class_id_for_workspace_type(
    db: &dyn NovaResolve,
    project: ProjectId,
    name: TypeName,
) -> Option<ClassId> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!(
        "query",
        name = "class_id_for_workspace_type",
        ?project,
        name = %name
    )
    .entered();

    cancel::check_cancelled(db);

    // Seed a deterministic bulk interning pass before mapping an individual
    // type. This ensures IDs are independent of query evaluation order.
    let map = db.workspace_class_id_map();
    let result = map.class_id_for_type(project, &name);

    db.record_query_stat("class_id_for_workspace_type", start.elapsed());
    result
}

fn jpms_compilation_env(
    db: &dyn NovaResolve,
    project: ProjectId,
) -> Option<ArcEq<nova_resolve::jpms_env::JpmsCompilationEnvironment>> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "jpms_compilation_env", ?project).entered();

    cancel::check_cancelled(db);

    let cfg = db.project_config(project);
    if !jpms_enabled(&cfg) {
        db.record_query_stat("jpms_compilation_env", start.elapsed());
        return None;
    }

    let jdk = db.jdk_index(project);
    let cache_dir = db.persistence().cache_dir().map(|dir| dir.classpath_dir());
    let env = nova_resolve::jpms_env::build_jpms_compilation_environment_for_project(
        &*jdk,
        &cfg,
        cache_dir.as_deref(),
    )
    .ok()
    .map(|env| ArcEq::new(Arc::new(env)));
    db.record_query_stat("jpms_compilation_env", start.elapsed());
    env
}

fn import_diagnostics(db: &dyn NovaResolve, file: FileId) -> Arc<Vec<Diagnostic>> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "import_diagnostics", ?file).entered();

    cancel::check_cancelled(db);

    let project = db.file_project(file);
    let workspace = db.workspace_def_map(project);
    let jdk = db.jdk_index(project);

    let cfg = db.project_config(project);
    let file_rel = db.file_rel_path(file);
    let jpms_enabled = jpms_enabled(&cfg);

    let import_map = db.import_map(file);

    let diags = if jpms_enabled {
        let env = db.jpms_compilation_env(project);
        if let Some(env) = env.as_deref() {
            let from = module_for_file(&cfg, file_rel.as_str());
            let index = JpmsProjectIndex {
                workspace: &workspace,
                graph: &env.env.graph,
                classpath: &env.classpath,
                jdk: &*jdk,
                from,
            };
            let resolver = nova_resolve::Resolver::new(&index).with_workspace(&workspace);
            resolver.diagnose_imports(&import_map)
        } else {
            Vec::new()
        }
    } else {
        let classpath = db.classpath_index(project);
        let index = WorkspaceFirstIndex {
            workspace: &workspace,
            classpath: classpath.as_deref(),
        };
        let resolver = nova_resolve::Resolver::new(&*jdk)
            .with_classpath(&index)
            .with_workspace(&workspace);
        resolver.diagnose_imports(&import_map)
    };

    let result = Arc::new(diags);
    db.record_query_stat("import_diagnostics", start.elapsed());
    result
}

fn resolve_name(
    db: &dyn NovaResolve,
    file: FileId,
    scope: nova_resolve::ScopeId,
    name: Name,
) -> Option<nova_resolve::Resolution> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span =
        tracing::debug_span!("query", name = "resolve_name", ?file, scope, name = %name).entered();

    cancel::check_cancelled(db);

    let project = db.file_project(file);
    let jdk = db.jdk_index(project);
    let workspace = db.workspace_def_map(project);
    let cfg = db.project_config(project);
    let file_rel = db.file_rel_path(file);

    let jpms_enabled = jpms_enabled(&cfg);

    let resolved = if jpms_enabled {
        let env = db.jpms_compilation_env(project);
        if let Some(env) = env.as_deref() {
            let from = module_for_file(&cfg, file_rel.as_str());
            let index = JpmsProjectIndex {
                workspace: &workspace,
                graph: &env.env.graph,
                classpath: &env.classpath,
                jdk: &*jdk,
                from,
            };
            let resolver = nova_resolve::Resolver::new(&index).with_workspace(&workspace);
            let built = db.scope_graph(file);
            resolver.resolve_name(&built.scopes, scope, &name)
        } else {
            None
        }
    } else {
        let classpath = db.classpath_index(project);
        let index = WorkspaceFirstIndex {
            workspace: &workspace,
            classpath: classpath.as_deref(),
        };
        let resolver = nova_resolve::Resolver::new(&*jdk)
            .with_classpath(&index)
            .with_workspace(&workspace);
        let built = db.scope_graph(file);
        resolver.resolve_name(&built.scopes, scope, &name)
    };

    db.record_query_stat("resolve_name", start.elapsed());
    resolved
}

fn resolve_name_detailed(
    db: &dyn NovaResolve,
    file: FileId,
    scope: nova_resolve::ScopeId,
    name: Name,
) -> nova_resolve::NameResolution {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!(
        "query",
        name = "resolve_name_detailed",
        ?file,
        scope,
        name = %name
    )
    .entered();

    cancel::check_cancelled(db);

    let project = db.file_project(file);
    let jdk = db.jdk_index(project);
    let workspace = db.workspace_def_map(project);
    let cfg = db.project_config(project);
    let file_rel = db.file_rel_path(file);

    let jpms_enabled = jpms_enabled(&cfg);

    let resolved = if jpms_enabled {
        let env = db.jpms_compilation_env(project);
        if let Some(env) = env.as_deref() {
            let from = module_for_file(&cfg, file_rel.as_str());
            let index = JpmsProjectIndex {
                workspace: &workspace,
                graph: &env.env.graph,
                classpath: &env.classpath,
                jdk: &*jdk,
                from,
            };
            let resolver = nova_resolve::Resolver::new(&index).with_workspace(&workspace);
            let built = db.scope_graph(file);
            resolver.resolve_name_detailed(&built.scopes, scope, &name)
        } else {
            nova_resolve::NameResolution::Unresolved
        }
    } else {
        let classpath = db.classpath_index(project);
        let index = WorkspaceFirstIndex {
            workspace: &workspace,
            classpath: classpath.as_deref(),
        };
        let resolver = nova_resolve::Resolver::new(&*jdk)
            .with_classpath(&index)
            .with_workspace(&workspace);
        let built = db.scope_graph(file);
        resolver.resolve_name_detailed(&built.scopes, scope, &name)
    };

    db.record_query_stat("resolve_name_detailed", start.elapsed());
    resolved
}
struct WorkspaceFirstIndex<'a> {
    workspace: &'a nova_resolve::WorkspaceDefMap,
    classpath: Option<&'a nova_classpath::ClasspathIndex>,
}

impl nova_core::TypeIndex for WorkspaceFirstIndex<'_> {
    fn resolve_type(&self, name: &nova_core::QualifiedName) -> Option<nova_core::TypeName> {
        self.workspace
            .resolve_type(name)
            .or_else(|| self.classpath.and_then(|cp| cp.resolve_type(name)))
    }

    fn resolve_type_in_package(
        &self,
        package: &nova_core::PackageName,
        name: &Name,
    ) -> Option<nova_core::TypeName> {
        self.workspace
            .resolve_type_in_package(package, name)
            .or_else(|| {
                self.classpath
                    .and_then(|cp| cp.resolve_type_in_package(package, name))
            })
    }

    fn package_exists(&self, package: &nova_core::PackageName) -> bool {
        self.workspace.package_exists(package)
            || self.classpath.is_some_and(|cp| cp.package_exists(package))
    }

    fn resolve_static_member(
        &self,
        owner: &nova_core::TypeName,
        name: &Name,
    ) -> Option<nova_core::StaticMemberId> {
        self.workspace
            .resolve_static_member(owner, name)
            .or_else(|| {
                self.classpath
                    .and_then(|cp| cp.resolve_static_member(owner, name))
            })
    }
}
