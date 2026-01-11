use std::sync::Arc;
use std::time::Instant;

use nova_core::Name;

use crate::FileId;

use super::cancellation as cancel;
use super::hir::NovaHir;
use super::stats::HasQueryStats;

#[ra_salsa::query_group(NovaResolveStorage)]
pub trait NovaResolve: NovaHir + HasQueryStats {
    /// Best-effort per-file semantic model suitable for building scopes.
    fn compilation_unit(&self, file: FileId) -> Arc<nova_hir::CompilationUnit>;

    /// Build the scope graph for a file.
    fn scope_graph(&self, file: FileId) -> Arc<nova_resolve::ScopeBuildResult>;

    /// Resolve `name` starting from `scope`.
    fn resolve_name(
        &self,
        file: FileId,
        scope: nova_resolve::ScopeId,
        name: Name,
    ) -> Option<nova_resolve::Resolution>;
}

fn compilation_unit(db: &dyn NovaResolve, file: FileId) -> Arc<nova_hir::CompilationUnit> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "compilation_unit", ?file).entered();

    cancel::check_cancelled(db);

    let parsed = db.java_parse(file);
    let lowered = lower_compilation_unit(parsed.compilation_unit());

    let result = Arc::new(lowered);
    db.record_query_stat("compilation_unit", start.elapsed());
    result
}

fn scope_graph(db: &dyn NovaResolve, file: FileId) -> Arc<nova_resolve::ScopeBuildResult> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "scope_graph", ?file).entered();

    cancel::check_cancelled(db);

    let project = db.file_project(file);
    let jdk = db.jdk_index(project);
    let classpath = db.classpath_index(project);

    let resolver = match classpath.as_deref() {
        Some(cp) => nova_resolve::Resolver::new(&*jdk).with_classpath(cp),
        None => nova_resolve::Resolver::new(&*jdk),
    };

    let unit = db.compilation_unit(file);
    let built =
        nova_resolve::build_scopes_with_resolver_and_cancel(&resolver, unit.as_ref(), || {
            cancel::check_cancelled(db);
        });

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

fn lower_compilation_unit(
    unit: &nova_syntax::java::ast::CompilationUnit,
) -> nova_hir::CompilationUnit {
    use nova_core::PackageName;

    let package = unit
        .package
        .as_ref()
        .map(|pkg| PackageName::from_dotted(&pkg.name));
    let mut out = nova_hir::CompilationUnit::new(package);

    for import in &unit.imports {
        if let Some(lowered) = lower_import(import) {
            out.imports.push(lowered);
        }
    }

    for ty in &unit.types {
        out.types.push(lower_type_decl(ty));
    }

    out
}

fn lower_import(import: &nova_syntax::java::ast::ImportDecl) -> Option<nova_hir::ImportDecl> {
    use nova_core::{PackageName, QualifiedName};
    use nova_hir::ImportDecl;

    if import.is_static {
        if import.is_star {
            return Some(ImportDecl::StaticStar {
                ty: QualifiedName::from_dotted(&import.path),
            });
        }

        let (owner, member) = import.path.rsplit_once('.')?;
        return Some(ImportDecl::StaticSingle {
            ty: QualifiedName::from_dotted(owner),
            member: Name::from(member),
            alias: None,
        });
    }

    if import.is_star {
        Some(ImportDecl::TypeStar {
            package: PackageName::from_dotted(&import.path),
        })
    } else {
        Some(ImportDecl::TypeSingle {
            ty: QualifiedName::from_dotted(&import.path),
            alias: None,
        })
    }
}

fn lower_type_decl(decl: &nova_syntax::java::ast::TypeDecl) -> nova_hir::TypeDecl {
    use nova_hir::{FieldDecl, MethodDecl, ParamDecl, TypeDecl};
    use nova_syntax::java::ast as syntax;

    let mut out = TypeDecl::new(decl.name());

    for member in decl.members() {
        match member {
            syntax::MemberDecl::Field(field) => {
                out.fields.push(FieldDecl::new(field.name.as_str()))
            }
            syntax::MemberDecl::Method(method) => {
                let mut hir_method = MethodDecl::new(method.name.as_str());
                hir_method.params = method
                    .params
                    .iter()
                    .map(|param| ParamDecl::new(param.name.as_str()))
                    .collect();

                if let Some(body) = &method.body {
                    hir_method.body = lower_block(body);
                }

                out.methods.push(hir_method);
            }
            syntax::MemberDecl::Type(ty) => out.nested_types.push(lower_type_decl(ty)),
            syntax::MemberDecl::Constructor(_) | syntax::MemberDecl::Initializer(_) => {}
        }
    }

    out
}

fn lower_block(block: &nova_syntax::java::ast::Block) -> nova_hir::Block {
    use nova_hir::{Block, LocalVarDecl, Stmt};
    use nova_syntax::java::ast as syntax;

    let mut out = Block { stmts: Vec::new() };
    for stmt in &block.statements {
        match stmt {
            syntax::Stmt::LocalVar(local) => {
                out.stmts
                    .push(Stmt::Local(LocalVarDecl::new(local.name.as_str())));
            }
            syntax::Stmt::Block(inner) => {
                out.stmts.push(Stmt::Block(lower_block(inner)));
            }
            _ => {}
        }
    }
    out
}
