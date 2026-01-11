use std::sync::Arc;
use std::time::Instant;

use nova_core::Name;

use crate::FileId;

use super::cancellation as cancel;
use super::hir::NovaHir;
use super::stats::HasQueryStats;

#[ra_salsa::query_group(NovaResolveStorage)]
pub trait NovaResolve: NovaHir + HasQueryStats {
    /// Build the scope graph for a file.
    fn scope_graph(&self, file: FileId) -> Arc<nova_resolve::ItemTreeScopeBuildResult>;

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
    let classpath = db.classpath_index(project);

    let resolver = match classpath.as_deref() {
        Some(cp) => nova_resolve::Resolver::new(&*jdk).with_classpath(cp),
        None => nova_resolve::Resolver::new(&*jdk),
    };

    let built = db.scope_graph(file);
    let resolved = resolver.resolve_name(&built.scopes, scope, &name);
    db.record_query_stat("resolve_name", start.elapsed());
    resolved
}
