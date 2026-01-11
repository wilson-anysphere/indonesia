use std::sync::Arc;
use std::time::Instant;

use nova_core::Name;
use nova_modules::{ModuleGraph, ModuleName};
use nova_project::ProjectConfig;
use nova_types::Diagnostic;

use crate::{FileId, ProjectId};

use super::cancellation as cancel;
use super::hir::NovaHir;
use super::stats::HasQueryStats;
use super::ArcEq;

#[ra_salsa::query_group(NovaResolveStorage)]
pub trait NovaResolve: NovaHir + HasQueryStats {
    /// Build the scope graph for a file.
    fn scope_graph(&self, file: FileId) -> Arc<nova_resolve::ItemTreeScopeBuildResult>;

    /// File-level definition map used for workspace-wide name resolution.
    fn def_map(&self, file: FileId) -> Arc<nova_resolve::DefMap>;

    /// Import declarations for a file lowered into an [`nova_resolve::ImportMap`].
    fn import_map(&self, file: FileId) -> Arc<nova_resolve::ImportMap>;

    /// Workspace-wide type namespace for a project.
    fn workspace_def_map(&self, project: ProjectId) -> Arc<nova_resolve::WorkspaceDefMap>;

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
}

fn scope_graph(db: &dyn NovaResolve, file: FileId) -> Arc<nova_resolve::ItemTreeScopeBuildResult> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "scope_graph", ?file).entered();

    cancel::check_cancelled(db);
    let tree = db.hir_item_tree(file);
    let built = nova_resolve::build_scopes_for_item_tree(file, &tree);

    let result = Arc::new(built);
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
    db.record_query_stat("workspace_def_map", start.elapsed());
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
    let env =
        nova_resolve::jpms_env::build_jpms_compilation_environment_for_project(&*jdk, &cfg, None)
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

fn jpms_enabled(cfg: &ProjectConfig) -> bool {
    !cfg.jpms_modules.is_empty() || cfg.jpms_workspace.is_some() || !cfg.module_path.is_empty()
}

fn module_for_file(cfg: &ProjectConfig, rel_path: &str) -> ModuleName {
    if cfg.jpms_modules.is_empty() {
        return ModuleName::unnamed();
    }

    let file_path = cfg.workspace_root.join(rel_path);
    let mut best: Option<(usize, ModuleName)> = None;
    for root in &cfg.jpms_modules {
        if !file_path.starts_with(&root.root) {
            continue;
        }
        let depth = root.root.components().count();
        let replace = match &best {
            Some((best_depth, _)) => depth > *best_depth,
            None => true,
        };
        if replace {
            best = Some((depth, root.name.clone()));
        }
    }

    best.map(|(_, name)| name)
        .unwrap_or_else(ModuleName::unnamed)
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

struct JpmsProjectIndex<'a> {
    workspace: &'a nova_resolve::WorkspaceDefMap,
    graph: &'a ModuleGraph,
    classpath: &'a nova_classpath::ModuleAwareClasspathIndex,
    jdk: &'a nova_jdk::JdkIndex,
    from: ModuleName,
}

impl<'a> JpmsProjectIndex<'a> {
    fn module_of_type(&self, ty: &nova_core::TypeName) -> Option<ModuleName> {
        if let Some(item) = self.workspace.item_by_type_name(ty) {
            if let Some(module) = self.workspace.module_for_item(item) {
                return Some(module.clone());
            }
            return Some(ModuleName::unnamed());
        }

        if let Some(to) = self.classpath.module_of(ty.as_str()) {
            return Some(to.clone());
        }

        if self.classpath.types.lookup_binary(ty.as_str()).is_some() {
            return Some(ModuleName::unnamed());
        }

        self.jdk.module_of_type(ty.as_str())
    }

    fn type_is_accessible(&self, ty: &nova_core::TypeName) -> bool {
        let Some(to) = self.module_of_type(ty) else {
            return true;
        };

        if !self.graph.can_read(&self.from, &to) {
            return false;
        }

        let package = ty
            .as_str()
            .rsplit_once('.')
            .map(|(pkg, _)| pkg)
            .unwrap_or("");

        let Some(info) = self.graph.get(&to) else {
            return true;
        };

        info.exports_package_to(package, &self.from)
    }
}

impl nova_core::TypeIndex for JpmsProjectIndex<'_> {
    fn resolve_type(&self, name: &nova_core::QualifiedName) -> Option<nova_core::TypeName> {
        if let Some(ty) = self.workspace.resolve_type(name) {
            if self.type_is_accessible(&ty) {
                return Some(ty);
            }
        }

        if let Some(ty) = self.classpath.resolve_type(name) {
            if self.type_is_accessible(&ty) {
                return Some(ty);
            }
        }

        let ty = self.jdk.resolve_type(name)?;
        self.type_is_accessible(&ty).then_some(ty)
    }

    fn resolve_type_in_package(
        &self,
        package: &nova_core::PackageName,
        name: &Name,
    ) -> Option<nova_core::TypeName> {
        if let Some(ty) = self.workspace.resolve_type_in_package(package, name) {
            if self.type_is_accessible(&ty) {
                return Some(ty);
            }
        }

        if let Some(ty) = self.classpath.resolve_type_in_package(package, name) {
            if self.type_is_accessible(&ty) {
                return Some(ty);
            }
        }

        let ty = self.jdk.resolve_type_in_package(package, name)?;
        self.type_is_accessible(&ty).then_some(ty)
    }

    fn package_exists(&self, package: &nova_core::PackageName) -> bool {
        self.workspace.package_exists(package)
            || self.classpath.package_exists(package)
            || self.jdk.package_exists(package)
    }

    fn resolve_static_member(
        &self,
        owner: &nova_core::TypeName,
        name: &Name,
    ) -> Option<nova_core::StaticMemberId> {
        if !self.type_is_accessible(owner) {
            return None;
        }

        self.workspace
            .resolve_static_member(owner, name)
            .or_else(|| self.classpath.resolve_static_member(owner, name))
            .or_else(|| self.jdk.resolve_static_member(owner, name))
    }
}
