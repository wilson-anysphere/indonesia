use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use nova_core::{Name, QualifiedName};
use nova_hir::hir::{
    AssignOp, BinaryOp, Body as HirBody, Expr as HirExpr, ExprId as HirExprId, LambdaBody,
    LiteralKind, Stmt as HirStmt, UnaryOp,
};
use nova_hir::ids::{FieldId, MethodId};
use nova_hir::item_tree::Modifiers;
use nova_resolve::expr_scopes::{ExprScopes, ResolvedValue as ResolvedLocal};
use nova_resolve::ids::{DefWithBodyId, ParamId};
use nova_resolve::{NameResolution, Resolution, ScopeKind, StaticMemberResolution, TypeResolution};
use nova_types::{
    assignment_conversion, binary_numeric_promotion, format_type, CallKind, ClassDef, ClassKind,
    Diagnostic, FieldDef, MethodCall, MethodDef, MethodResolution, PrimitiveType, ResolvedMethod,
    Span, TyContext, Type, TypeEnv, TypeProvider, TypeStore, TypeVarId,
};
use nova_types_bridge::ExternalTypeLoader;

use crate::FileId;

use super::cancellation as cancel;
use super::resolve::NovaResolve;
use super::stats::HasQueryStats;
use super::ArcEq;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileExprId {
    pub owner: DefWithBodyId,
    pub expr: HirExprId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyTypeckResult {
    pub env: ArcEq<TypeStore>,
    pub expr_types: Vec<Type>,
    pub call_resolutions: Vec<Option<ResolvedMethod>>,
    pub diagnostics: Vec<Diagnostic>,
    pub expected_return: Type,
}

#[ra_salsa::query_group(NovaTypeckStorage)]
pub trait NovaTypeck: NovaResolve + HasQueryStats {
    fn typeck_body(&self, owner: DefWithBodyId) -> Arc<BodyTypeckResult>;

    fn type_of_expr(&self, file: FileId, expr: FileExprId) -> Type;
    fn type_of_def(&self, def: DefWithBodyId) -> Type;

    fn resolve_method_call(&self, file: FileId, call_site: FileExprId) -> Option<ResolvedMethod>;
    fn type_diagnostics(&self, file: FileId) -> Vec<Diagnostic>;

    /// Best-effort helper used by IDE features: infer the type of the smallest expression that
    /// encloses `offset` and return a formatted string.
    fn type_at_offset_display(&self, file: FileId, offset: u32) -> Option<String>;
}

fn type_of_expr(db: &dyn NovaTypeck, _file: FileId, expr: FileExprId) -> Type {
    let body = db.typeck_body(expr.owner);
    body.expr_types
        .get(expr.expr.idx())
        .cloned()
        .unwrap_or(Type::Unknown)
}

fn type_of_def(db: &dyn NovaTypeck, def: DefWithBodyId) -> Type {
    db.typeck_body(def).expected_return.clone()
}

fn resolve_method_call(
    db: &dyn NovaTypeck,
    _file: FileId,
    call_site: FileExprId,
) -> Option<ResolvedMethod> {
    let body = db.typeck_body(call_site.owner);
    body.call_resolutions
        .get(call_site.expr.idx())
        .and_then(|m| m.clone())
}

fn type_diagnostics(db: &dyn NovaTypeck, file: FileId) -> Vec<Diagnostic> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "type_diagnostics", ?file).entered();

    cancel::check_cancelled(db);

    let tree = db.hir_item_tree(file);
    let mut diags = Vec::new();
    let owners = collect_body_owners(&tree);
    for (idx, owner) in owners.iter().enumerate() {
        cancel::checkpoint_cancelled_every(db, idx as u32, 32);
        diags.extend(db.typeck_body(*owner).diagnostics.iter().cloned());
    }

    diags.sort_by_key(|d| {
        (
            d.span.map(|s| s.start).unwrap_or(usize::MAX),
            d.message.clone(),
        )
    });

    db.record_query_stat("type_diagnostics", start.elapsed());
    diags
}

fn type_at_offset_display(db: &dyn NovaTypeck, file: FileId, offset: u32) -> Option<String> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span =
        tracing::debug_span!("query", name = "type_at_offset_display", ?file, offset).entered();

    cancel::check_cancelled(db);

    let tree = db.hir_item_tree(file);
    let owners = collect_body_owners(&tree);

    let mut best: Option<(DefWithBodyId, HirExprId, usize)> = None;
    for (idx, owner) in owners.iter().enumerate() {
        cancel::checkpoint_cancelled_every(db, idx as u32, 32);
        let body = match *owner {
            DefWithBodyId::Method(m) => db.hir_body(m),
            DefWithBodyId::Constructor(c) => db.hir_constructor_body(c),
            DefWithBodyId::Initializer(i) => db.hir_initializer_body(i),
        };

        let offset_usize = offset as usize;
        find_best_expr_in_stmt(&body, body.root, offset_usize, *owner, &mut best);
    }

    let (owner, expr, _) = best?;
    let body_res = db.typeck_body(owner);
    let ty = body_res
        .expr_types
        .get(expr.idx())
        .cloned()
        .unwrap_or(Type::Unknown);
    let rendered = format_type(&*body_res.env, &ty);

    db.record_query_stat("type_at_offset_display", start.elapsed());
    Some(rendered)
}

fn typeck_body(db: &dyn NovaTypeck, owner: DefWithBodyId) -> Arc<BodyTypeckResult> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "typeck_body", ?owner).entered();

    cancel::check_cancelled(db);

    let file = def_file(owner);
    let project = db.file_project(file);
    let jdk = db.jdk_index(project);
    let classpath = db.classpath_index(project);

    let resolver = match classpath.as_deref() {
        Some(cp) => nova_resolve::Resolver::new(&*jdk).with_classpath(cp),
        None => nova_resolve::Resolver::new(&*jdk),
    };

    let scopes = db.scope_graph(file);
    let body_scope = match owner {
        DefWithBodyId::Method(m) => scopes.method_scopes.get(&m).copied(),
        DefWithBodyId::Constructor(c) => scopes.constructor_scopes.get(&c).copied(),
        DefWithBodyId::Initializer(i) => scopes.initializer_scopes.get(&i).copied(),
    }
    .unwrap_or(scopes.file_scope);

    let tree = db.hir_item_tree(file);
    let body = match owner {
        DefWithBodyId::Method(m) => db.hir_body(m),
        DefWithBodyId::Constructor(c) => db.hir_constructor_body(c),
        DefWithBodyId::Initializer(i) => db.hir_initializer_body(i),
    };

    let param_ids = params_for_owner(&tree, owner);
    let expr_scopes = ExprScopes::new(&body, &param_ids, |id| param_name_lookup(&tree, id));

    // Build an env for this body.
    let mut store = TypeStore::with_minimal_jdk();
    let provider = match classpath.as_deref() {
        Some(cp) => nova_types::ChainTypeProvider::new(vec![
            cp as &dyn TypeProvider,
            &*jdk as &dyn TypeProvider,
        ]),
        None => nova_types::ChainTypeProvider::new(vec![&*jdk as &dyn TypeProvider]),
    };
    let mut loader = ExternalTypeLoader::new(&mut store, &provider);

    // Define source types in this file so `Type::Class` ids are stable within this body.
    let (field_types, method_types) = define_source_types(&resolver, &scopes, &tree, &mut loader);

    let (expected_return, signature_diags) = resolve_expected_return_type(
        &resolver,
        &scopes.scopes,
        body_scope,
        &tree,
        owner,
        &mut loader,
    );
    let (param_types, param_diags) = resolve_param_types(
        &resolver,
        &scopes.scopes,
        body_scope,
        &tree,
        owner,
        &mut loader,
    );

    let mut checker = BodyChecker::new(
        db,
        owner,
        &resolver,
        &scopes.scopes,
        body_scope,
        &tree,
        &body,
        expr_scopes,
        expected_return.clone(),
        param_types,
        field_types,
        method_types,
    );
    checker.diagnostics.extend(signature_diags);
    checker.diagnostics.extend(param_diags);

    checker.check_body(&mut loader);

    // Finalize expression type table.
    let mut expr_types = Vec::with_capacity(body.exprs.len());
    for idx in 0..body.exprs.len() {
        cancel::checkpoint_cancelled_every(db, idx as u32, 128);
        if let Some(info) = checker.expr_info.get(idx).and_then(|i| i.clone()) {
            expr_types.push(info.ty);
        } else {
            // If an expr was unreachable due to parse recovery, still provide a stable entry.
            expr_types.push(Type::Unknown);
        }
    }

    let call_resolutions = checker.call_resolutions;
    let diagnostics = checker.diagnostics;

    drop(loader);
    let env = ArcEq::new(Arc::new(store));

    let result = Arc::new(BodyTypeckResult {
        env,
        expr_types,
        call_resolutions,
        diagnostics,
        expected_return,
    });

    db.record_query_stat("typeck_body", start.elapsed());
    result
}

#[derive(Debug, Clone)]
struct ExprInfo {
    ty: Type,
    is_type_ref: bool,
}

struct BodyChecker<'a, 'idx> {
    db: &'a dyn NovaTypeck,
    owner: DefWithBodyId,
    resolver: &'a nova_resolve::Resolver<'idx>,
    scopes: &'a nova_resolve::ScopeGraph,
    scope_id: nova_resolve::ScopeId,
    tree: &'a nova_hir::item_tree::ItemTree,
    body: &'a HirBody,
    expr_scopes: ExprScopes,
    expected_return: Type,
    local_types: Vec<Type>,
    param_types: Vec<Type>,
    field_types: HashMap<FieldId, Type>,
    method_types: HashMap<MethodId, (Vec<Type>, Type)>,
    expr_info: Vec<Option<ExprInfo>>,
    call_resolutions: Vec<Option<ResolvedMethod>>,
    diagnostics: Vec<Diagnostic>,
    steps: u32,
}

impl<'a, 'idx> BodyChecker<'a, 'idx> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        db: &'a dyn NovaTypeck,
        owner: DefWithBodyId,
        resolver: &'a nova_resolve::Resolver<'idx>,
        scopes: &'a nova_resolve::ScopeGraph,
        scope_id: nova_resolve::ScopeId,
        tree: &'a nova_hir::item_tree::ItemTree,
        body: &'a HirBody,
        expr_scopes: ExprScopes,
        expected_return: Type,
        param_types: Vec<Type>,
        field_types: HashMap<FieldId, Type>,
        method_types: HashMap<MethodId, (Vec<Type>, Type)>,
    ) -> Self {
        let local_types = vec![Type::Unknown; body.locals.len()];
        let expr_info = vec![None; body.exprs.len()];
        let call_resolutions = vec![None; body.exprs.len()];

        Self {
            db,
            owner,
            resolver,
            scopes,
            scope_id,
            tree,
            body,
            expr_scopes,
            expected_return,
            local_types,
            param_types,
            field_types,
            method_types,
            expr_info,
            call_resolutions,
            diagnostics: Vec::new(),
            steps: 0,
        }
    }

    fn check_body(&mut self, loader: &mut ExternalTypeLoader<'_>) {
        self.check_stmt(loader, self.body.root);
    }

    fn check_stmt(&mut self, loader: &mut ExternalTypeLoader<'_>, stmt: nova_hir::hir::StmtId) {
        self.tick();
        match &self.body.stmts[stmt] {
            HirStmt::Block { statements, .. } => {
                for &stmt in statements {
                    self.check_stmt(loader, stmt);
                }
            }
            HirStmt::Let {
                local, initializer, ..
            } => {
                let data = &self.body.locals[*local];

                // Handle `var` inference.
                if data.ty_text.trim() == "var" {
                    if let Some(init) = initializer {
                        let init_ty = self.infer_expr(loader, *init).ty;
                        self.local_types[local.idx()] = init_ty;
                    }
                    return;
                }

                let decl_ty =
                    self.resolve_source_type(loader, data.ty_text.as_str(), Some(data.ty_range));
                self.local_types[local.idx()] = decl_ty.clone();

                let Some(init) = initializer else {
                    return;
                };

                let init_ty = self.infer_expr(loader, *init).ty;

                if decl_ty.is_errorish() || init_ty.is_errorish() {
                    return;
                }

                let env_ro: &dyn TypeEnv = &*loader.store;
                if assignment_conversion(env_ro, &init_ty, &decl_ty).is_none() {
                    let expected = format_type(env_ro, &decl_ty);
                    let found = format_type(env_ro, &init_ty);
                    self.diagnostics.push(Diagnostic::error(
                        "type-mismatch",
                        format!("type mismatch: expected {expected}, found {found}"),
                        Some(self.body.exprs[*init].range()),
                    ));
                }
            }
            HirStmt::Expr { expr, .. } => {
                let _ = self.infer_expr(loader, *expr);
            }
            HirStmt::Return { expr, .. } => {
                let Some(expr) = expr else {
                    return;
                };
                let expr_ty = self.infer_expr(loader, *expr).ty;
                if self.expected_return == Type::Void {
                    self.diagnostics.push(Diagnostic::error(
                        "return-mismatch",
                        "cannot return a value from a `void` method",
                        Some(self.body.exprs[*expr].range()),
                    ));
                    return;
                }

                if expr_ty.is_errorish() || self.expected_return.is_errorish() {
                    return;
                }

                let env_ro: &dyn TypeEnv = &*loader.store;
                if assignment_conversion(env_ro, &expr_ty, &self.expected_return).is_none() {
                    let expected = format_type(env_ro, &self.expected_return);
                    let found = format_type(env_ro, &expr_ty);
                    self.diagnostics.push(Diagnostic::error(
                        "return-mismatch",
                        format!("return type mismatch: expected {expected}, found {found}"),
                        Some(self.body.exprs[*expr].range()),
                    ));
                }
            }
            HirStmt::If {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                let _ = self.infer_expr(loader, *condition);
                self.check_stmt(loader, *then_branch);
                if let Some(else_branch) = else_branch {
                    self.check_stmt(loader, *else_branch);
                }
            }
            HirStmt::While {
                condition, body, ..
            } => {
                let _ = self.infer_expr(loader, *condition);
                self.check_stmt(loader, *body);
            }
            HirStmt::For {
                init,
                condition,
                update,
                body,
                ..
            } => {
                for stmt in init {
                    self.check_stmt(loader, *stmt);
                }
                if let Some(condition) = condition {
                    let _ = self.infer_expr(loader, *condition);
                }
                for expr in update {
                    let _ = self.infer_expr(loader, *expr);
                }
                self.check_stmt(loader, *body);
            }
            HirStmt::ForEach {
                local,
                iterable,
                body,
                ..
            } => {
                let data = &self.body.locals[*local];
                if data.ty_text.trim() != "var" {
                    let decl_ty = self.resolve_source_type(
                        loader,
                        data.ty_text.as_str(),
                        Some(data.ty_range),
                    );
                    self.local_types[local.idx()] = decl_ty;
                }

                let _ = self.infer_expr(loader, *iterable);
                self.check_stmt(loader, *body);
            }
            HirStmt::Switch { selector, body, .. } => {
                let _ = self.infer_expr(loader, *selector);
                self.check_stmt(loader, *body);
            }
            HirStmt::Try {
                body,
                catches,
                finally,
                ..
            } => {
                self.check_stmt(loader, *body);
                for catch in catches {
                    let data = &self.body.locals[catch.param];
                    if data.ty_text.trim() != "var" {
                        let catch_ty = self.resolve_source_type(
                            loader,
                            data.ty_text.as_str(),
                            Some(data.ty_range),
                        );
                        self.local_types[catch.param.idx()] = catch_ty;
                    }
                    self.check_stmt(loader, catch.body);
                }
                if let Some(finally) = finally {
                    self.check_stmt(loader, *finally);
                }
            }
            HirStmt::Throw { expr, .. } => {
                let _ = self.infer_expr(loader, *expr);
            }
            HirStmt::Break { .. } | HirStmt::Continue { .. } => {}
            HirStmt::Empty { .. } => {}
        }
    }

    fn infer_expr(&mut self, loader: &mut ExternalTypeLoader<'_>, expr: HirExprId) -> ExprInfo {
        if let Some(info) = self.expr_info[expr.idx()].clone() {
            return info;
        }
        self.tick();

        let info = match &self.body.exprs[expr] {
            HirExpr::Missing { .. } => ExprInfo {
                ty: Type::Unknown,
                is_type_ref: false,
            },
            HirExpr::Literal { kind, .. } => match kind {
                LiteralKind::Int => ExprInfo {
                    ty: Type::Primitive(PrimitiveType::Int),
                    is_type_ref: false,
                },
                LiteralKind::String => ExprInfo {
                    ty: Type::class(loader.store.well_known().string, vec![]),
                    is_type_ref: false,
                },
                LiteralKind::Bool => ExprInfo {
                    ty: Type::Primitive(PrimitiveType::Boolean),
                    is_type_ref: false,
                },
            },
            HirExpr::Null { .. } => ExprInfo {
                ty: Type::Null,
                is_type_ref: false,
            },
            HirExpr::This { .. } | HirExpr::Super { .. } => ExprInfo {
                ty: Type::Unknown,
                is_type_ref: false,
            },
            HirExpr::Name { name, range } => self.infer_name(loader, expr, name.as_str(), *range),
            HirExpr::FieldAccess { receiver, name, .. } => {
                self.infer_field_access(loader, *receiver, name.as_str(), expr)
            }
            HirExpr::MethodReference { receiver, .. } => {
                let _ = self.infer_expr(loader, *receiver);
                ExprInfo {
                    ty: Type::Unknown,
                    is_type_ref: false,
                }
            }
            HirExpr::ConstructorReference { receiver, .. } => {
                let _ = self.infer_expr(loader, *receiver);
                ExprInfo {
                    ty: Type::Unknown,
                    is_type_ref: false,
                }
            }
            HirExpr::ClassLiteral { ty, .. } => {
                let _ = self.infer_expr(loader, *ty);
                ExprInfo {
                    ty: Type::Unknown,
                    is_type_ref: false,
                }
            }
            HirExpr::Call { callee, args, .. } => self.infer_call(loader, *callee, args, expr),
            HirExpr::New {
                class,
                class_range,
                args,
                ..
            } => {
                for arg in args {
                    let _ = self.infer_expr(loader, *arg);
                }

                ExprInfo {
                    ty: self.resolve_source_type(loader, class.as_str(), Some(*class_range)),
                    is_type_ref: false,
                }
            }
            HirExpr::Unary { op, expr, .. } => {
                let inner = self.infer_expr(loader, *expr).ty;
                let ty = match op {
                    UnaryOp::Not => Type::Primitive(PrimitiveType::Boolean),
                    UnaryOp::Plus | UnaryOp::Minus | UnaryOp::BitNot => match inner {
                        Type::Primitive(primitive) if primitive.is_numeric() => {
                            // Unary numeric promotion.
                            match primitive {
                                PrimitiveType::Byte
                                | PrimitiveType::Short
                                | PrimitiveType::Char => Type::Primitive(PrimitiveType::Int),
                                _ => Type::Primitive(primitive),
                            }
                        }
                        _ => Type::Unknown,
                    },
                    UnaryOp::PreInc | UnaryOp::PreDec | UnaryOp::PostInc | UnaryOp::PostDec => {
                        inner
                    }
                };
                ExprInfo {
                    ty,
                    is_type_ref: false,
                }
            }
            HirExpr::Binary { op, lhs, rhs, .. } => self.infer_binary(loader, *op, *lhs, *rhs),
            HirExpr::Assign { lhs, rhs, op, .. } => {
                let lhs_info = self.infer_expr(loader, *lhs);
                let rhs_info = self.infer_expr(loader, *rhs);

                // Best-effort: assignment expression type is the LHS type.
                // For compound assignments, Java applies numeric promotion and an implicit cast.
                let ty = match op {
                    AssignOp::Assign => lhs_info.ty.clone(),
                    _ => {
                        if lhs_info.ty.is_errorish() {
                            rhs_info.ty
                        } else {
                            lhs_info.ty.clone()
                        }
                    }
                };

                ExprInfo {
                    ty,
                    is_type_ref: false,
                }
            }
            HirExpr::Conditional {
                condition,
                then_expr,
                else_expr,
                ..
            } => {
                let _ = self.infer_expr(loader, *condition);
                let then_ty = self.infer_expr(loader, *then_expr).ty;
                let else_ty = self.infer_expr(loader, *else_expr).ty;
                let ty = if then_ty == else_ty {
                    then_ty
                } else if then_ty.is_errorish() {
                    else_ty
                } else if else_ty.is_errorish() {
                    then_ty
                } else {
                    Type::Unknown
                };

                ExprInfo {
                    ty,
                    is_type_ref: false,
                }
            }
            HirExpr::Lambda { body, .. } => {
                match body {
                    LambdaBody::Expr(expr) => {
                        let _ = self.infer_expr(loader, *expr);
                    }
                    LambdaBody::Block(stmt) => {
                        self.check_stmt(loader, *stmt);
                    }
                }
                ExprInfo {
                    ty: Type::Unknown,
                    is_type_ref: false,
                }
            }
        };

        self.expr_info[expr.idx()] = Some(info.clone());
        info
    }

    fn infer_name(
        &mut self,
        loader: &mut ExternalTypeLoader<'_>,
        expr: HirExprId,
        name: &str,
        range: Span,
    ) -> ExprInfo {
        match name {
            "null" => {
                return ExprInfo {
                    ty: Type::Null,
                    is_type_ref: false,
                }
            }
            "true" | "false" => {
                return ExprInfo {
                    ty: Type::Primitive(PrimitiveType::Boolean),
                    is_type_ref: false,
                }
            }
            _ => {}
        }

        let scope = self
            .expr_scopes
            .scope_for_expr(expr)
            .unwrap_or_else(|| self.expr_scopes.root_scope());
        let resolved = self.expr_scopes.resolve_name(scope, &Name::from(name));
        if let Some(resolved) = resolved {
            match resolved {
                ResolvedLocal::Local(local) => {
                    return ExprInfo {
                        ty: self.local_types[local.idx()].clone(),
                        is_type_ref: false,
                    };
                }
                ResolvedLocal::Param(param) => {
                    let idx = param.index as usize;
                    return ExprInfo {
                        ty: self.param_types.get(idx).cloned().unwrap_or(Type::Unknown),
                        is_type_ref: false,
                    };
                }
            }
        }

        match self
            .resolver
            .resolve_name_detailed(self.scopes, self.scope_id, &Name::from(name))
        {
            NameResolution::Resolved(res) => self.resolution_to_expr(loader, res, range),
            NameResolution::Ambiguous(_) => {
                self.diagnostics.push(Diagnostic::error(
                    "ambiguous-name",
                    format!("ambiguous reference `{name}`"),
                    Some(range),
                ));
                ExprInfo {
                    ty: Type::Unknown,
                    is_type_ref: false,
                }
            }
            NameResolution::Unresolved => {
                self.diagnostics.push(Diagnostic::error(
                    "unresolved-name",
                    format!("unresolved reference `{name}`"),
                    Some(range),
                ));
                ExprInfo {
                    ty: Type::Unknown,
                    is_type_ref: false,
                }
            }
        }
    }

    fn resolution_to_expr(
        &mut self,
        loader: &mut ExternalTypeLoader<'_>,
        res: Resolution,
        range: Span,
    ) -> ExprInfo {
        match res {
            Resolution::Local(_) | Resolution::Parameter(_) => ExprInfo {
                ty: Type::Unknown,
                is_type_ref: false,
            },
            Resolution::Field(field) => ExprInfo {
                ty: self
                    .field_types
                    .get(&field)
                    .cloned()
                    .unwrap_or(Type::Unknown),
                is_type_ref: false,
            },
            Resolution::Methods(_) | Resolution::Constructors(_) => ExprInfo {
                ty: Type::Unknown,
                is_type_ref: false,
            },
            Resolution::Type(ty) => {
                let binary_name = match ty {
                    TypeResolution::External(name) => name.as_str().to_string(),
                    TypeResolution::Source(item) => self
                        .scopes
                        .type_name(item)
                        .map(|n| n.as_str().to_string())
                        .unwrap_or_else(|| "<unknown>".to_string()),
                };

                if let Some(id) = loader.ensure_class(&binary_name) {
                    ExprInfo {
                        ty: Type::class(id, vec![]),
                        is_type_ref: true,
                    }
                } else {
                    ExprInfo {
                        ty: Type::Named(binary_name),
                        is_type_ref: true,
                    }
                }
            }
            Resolution::Package(_) => ExprInfo {
                ty: Type::Unknown,
                is_type_ref: false,
            },
            Resolution::StaticMember(member) => self.static_member_to_expr(loader, member, range),
        }
    }

    fn static_member_to_expr(
        &mut self,
        loader: &mut ExternalTypeLoader<'_>,
        member: StaticMemberResolution,
        range: Span,
    ) -> ExprInfo {
        let StaticMemberResolution::External(id) = member else {
            return ExprInfo {
                ty: Type::Unknown,
                is_type_ref: false,
            };
        };

        let Some((owner, name)) = id.as_str().split_once("::") else {
            return ExprInfo {
                ty: Type::Unknown,
                is_type_ref: false,
            };
        };

        let receiver = loader
            .ensure_class(owner)
            .map(|id| Type::class(id, vec![]))
            .unwrap_or_else(|| Type::Named(owner.to_string()));

        {
            let env_ro: &dyn TypeEnv = &*loader.store;
            let mut ctx = TyContext::new(env_ro);
            if let Some(field) = ctx.resolve_field(&receiver, name, CallKind::Static) {
                return ExprInfo {
                    ty: field.ty,
                    is_type_ref: false,
                };
            }
        }

        self.diagnostics.push(Diagnostic::error(
            "unresolved-static-member",
            format!("unresolved static member `{owner}::{name}`"),
            Some(range),
        ));
        ExprInfo {
            ty: Type::Unknown,
            is_type_ref: false,
        }
    }

    fn infer_field_access(
        &mut self,
        loader: &mut ExternalTypeLoader<'_>,
        receiver: HirExprId,
        name: &str,
        expr: HirExprId,
    ) -> ExprInfo {
        let recv_info = self.infer_expr(loader, receiver);
        let recv_ty = recv_info.ty.clone();

        // Best-effort array `length` support.
        if !recv_info.is_type_ref && matches!(recv_ty, Type::Array(_)) && name == "length" {
            return ExprInfo {
                ty: Type::Primitive(PrimitiveType::Int),
                is_type_ref: false,
            };
        }

        ensure_type_loaded(loader, &recv_ty);

        if recv_info.is_type_ref {
            // Static access: field or nested type.
            let env_ro: &dyn TypeEnv = &*loader.store;
            let mut ctx = TyContext::new(env_ro);
            if let Some(field) = ctx.resolve_field(&recv_ty, name, CallKind::Static) {
                return ExprInfo {
                    ty: field.ty,
                    is_type_ref: false,
                };
            }

            // Nested class (binary `$` form).
            if let Some(binary) = type_binary_name(loader.store, &recv_ty) {
                let nested = format!("{binary}${name}");
                if let Some(id) = loader.ensure_class(&nested) {
                    return ExprInfo {
                        ty: Type::class(id, vec![]),
                        is_type_ref: true,
                    };
                }
            }
        } else {
            // Instance access.
            let env_ro: &dyn TypeEnv = &*loader.store;
            let mut ctx = TyContext::new(env_ro);
            if let Some(field) = ctx.resolve_field(&recv_ty, name, CallKind::Instance) {
                return ExprInfo {
                    ty: field.ty,
                    is_type_ref: false,
                };
            }
        }

        self.diagnostics.push(Diagnostic::error(
            "unresolved-field",
            format!("unresolved field `{name}`"),
            Some(self.body.exprs[expr].range()),
        ));
        ExprInfo {
            ty: Type::Unknown,
            is_type_ref: false,
        }
    }

    fn infer_call(
        &mut self,
        loader: &mut ExternalTypeLoader<'_>,
        callee: HirExprId,
        args: &[HirExprId],
        expr: HirExprId,
    ) -> ExprInfo {
        match &self.body.exprs[callee] {
            HirExpr::FieldAccess { receiver, name, .. } => {
                let recv_info = self.infer_expr(loader, *receiver);
                let call_kind = if recv_info.is_type_ref {
                    CallKind::Static
                } else {
                    CallKind::Instance
                };
                let recv_ty = recv_info.ty.clone();
                ensure_type_loaded(loader, &recv_ty);

                let arg_types = args
                    .iter()
                    .map(|arg| self.infer_expr(loader, *arg).ty)
                    .collect::<Vec<_>>();

                let call = MethodCall {
                    receiver: recv_ty,
                    call_kind,
                    name: name.as_str(),
                    args: arg_types,
                    expected_return: None,
                    explicit_type_args: Vec::new(),
                };

                let env_ro: &dyn TypeEnv = &*loader.store;
                let mut ctx = TyContext::new(env_ro);
                match nova_types::resolve_method_call(&mut ctx, &call) {
                    MethodResolution::Found(method) => {
                        self.call_resolutions[expr.idx()] = Some(method.clone());
                        ExprInfo {
                            ty: method.return_type,
                            is_type_ref: false,
                        }
                    }
                    MethodResolution::Ambiguous(amb) => {
                        self.diagnostics.push(Diagnostic::error(
                            "ambiguous-call",
                            format!("ambiguous call `{}`", name),
                            Some(self.body.exprs[expr].range()),
                        ));
                        if let Some(first) = amb.candidates.first() {
                            self.call_resolutions[expr.idx()] = Some(first.clone());
                            ExprInfo {
                                ty: first.return_type.clone(),
                                is_type_ref: false,
                            }
                        } else {
                            ExprInfo {
                                ty: Type::Unknown,
                                is_type_ref: false,
                            }
                        }
                    }
                    MethodResolution::NotFound(_) => {
                        self.diagnostics.push(Diagnostic::error(
                            "unresolved-method",
                            format!("unresolved method `{}`", name),
                            Some(self.body.exprs[expr].range()),
                        ));
                        ExprInfo {
                            ty: Type::Error,
                            is_type_ref: false,
                        }
                    }
                }
            }
            HirExpr::Name { name, range } => {
                let arg_types = args
                    .iter()
                    .map(|arg| self.infer_expr(loader, *arg).ty)
                    .collect::<Vec<_>>();

                // Unqualified calls like `foo()` are usually shorthand for `this.foo()`.
                // Resolve them against the enclosing class first (using the right
                // call kind for the current static/instance context), then fall back to
                // static-imported methods.
                if let Some(receiver_ty) = self.enclosing_class_type(loader) {
                    ensure_type_loaded(loader, &receiver_ty);

                    let is_static_context = self.is_static_context();
                    let call_kind = if is_static_context {
                        CallKind::Static
                    } else {
                        CallKind::Instance
                    };

                    let call = MethodCall {
                        receiver: receiver_ty.clone(),
                        call_kind,
                        name: name.as_str(),
                        args: arg_types.clone(),
                        expected_return: None,
                        explicit_type_args: Vec::new(),
                    };

                    let env_ro: &dyn TypeEnv = &*loader.store;
                    let mut ctx = TyContext::new(env_ro);
                    match nova_types::resolve_method_call(&mut ctx, &call) {
                        MethodResolution::Found(method) => {
                            self.call_resolutions[expr.idx()] = Some(method.clone());
                            return ExprInfo {
                                ty: method.return_type,
                                is_type_ref: false,
                            };
                        }
                        MethodResolution::Ambiguous(amb) => {
                            self.diagnostics.push(Diagnostic::error(
                                "ambiguous-call",
                                format!("ambiguous call `{}`", name),
                                Some(self.body.exprs[expr].range()),
                            ));
                            if let Some(first) = amb.candidates.first() {
                                self.call_resolutions[expr.idx()] = Some(first.clone());
                                return ExprInfo {
                                    ty: first.return_type.clone(),
                                    is_type_ref: false,
                                };
                            }
                            return ExprInfo {
                                ty: Type::Unknown,
                                is_type_ref: false,
                            };
                        }
                        MethodResolution::NotFound(_) => {}
                    }

                    if is_static_context {
                        // Best-effort: if this call *would* resolve in an instance context, emit a
                        // more precise diagnostic instead of falling back to static imports.
                        let call = MethodCall {
                            receiver: receiver_ty,
                            call_kind: CallKind::Instance,
                            name: name.as_str(),
                            args: arg_types.clone(),
                            expected_return: None,
                            explicit_type_args: Vec::new(),
                        };
                        let mut ctx = TyContext::new(env_ro);
                        match nova_types::resolve_method_call(&mut ctx, &call) {
                            MethodResolution::Found(_) | MethodResolution::Ambiguous(_) => {
                                self.diagnostics.push(Diagnostic::error(
                                    "unresolved-method",
                                    format!(
                                        "cannot call instance method `{}` from a static context",
                                        name
                                    ),
                                    Some(self.body.exprs[expr].range()),
                                ));
                                return ExprInfo {
                                    ty: Type::Error,
                                    is_type_ref: false,
                                };
                            }
                            MethodResolution::NotFound(_) => {}
                        }
                    }
                }

                // Handle static-imported methods.
                let NameResolution::Resolved(Resolution::StaticMember(
                    StaticMemberResolution::External(id),
                )) = self.resolver.resolve_name_detailed(
                    self.scopes,
                    self.scope_id,
                    &Name::from(name.as_str()),
                )
                else {
                    self.diagnostics.push(Diagnostic::error(
                        "unresolved-method",
                        format!("unresolved call `{}`", name),
                        Some(*range),
                    ));
                    return ExprInfo {
                        ty: Type::Unknown,
                        is_type_ref: false,
                    };
                };

                let Some((owner, member)) = id.as_str().split_once("::") else {
                    return ExprInfo {
                        ty: Type::Unknown,
                        is_type_ref: false,
                    };
                };

                let recv_ty = loader
                    .ensure_class(owner)
                    .map(|id| Type::class(id, vec![]))
                    .unwrap_or_else(|| Type::Named(owner.to_string()));
                ensure_type_loaded(loader, &recv_ty);

                let call = MethodCall {
                    receiver: recv_ty,
                    call_kind: CallKind::Static,
                    name: member,
                    args: arg_types,
                    expected_return: None,
                    explicit_type_args: Vec::new(),
                };

                let env_ro: &dyn TypeEnv = &*loader.store;
                let mut ctx = TyContext::new(env_ro);
                match nova_types::resolve_method_call(&mut ctx, &call) {
                    MethodResolution::Found(method) => {
                        self.call_resolutions[expr.idx()] = Some(method.clone());
                        ExprInfo {
                            ty: method.return_type,
                            is_type_ref: false,
                        }
                    }
                    MethodResolution::Ambiguous(amb) => {
                        self.diagnostics.push(Diagnostic::error(
                            "ambiguous-call",
                            format!("ambiguous call `{member}`"),
                            Some(self.body.exprs[expr].range()),
                        ));
                        if let Some(first) = amb.candidates.first() {
                            self.call_resolutions[expr.idx()] = Some(first.clone());
                            ExprInfo {
                                ty: first.return_type.clone(),
                                is_type_ref: false,
                            }
                        } else {
                            ExprInfo {
                                ty: Type::Unknown,
                                is_type_ref: false,
                            }
                        }
                    }
                    MethodResolution::NotFound(_) => {
                        self.diagnostics.push(Diagnostic::error(
                            "unresolved-method",
                            format!("unresolved method `{member}`"),
                            Some(self.body.exprs[expr].range()),
                        ));
                        ExprInfo {
                            ty: Type::Error,
                            is_type_ref: false,
                        }
                    }
                }
            }
            _ => ExprInfo {
                ty: Type::Unknown,
                is_type_ref: false,
            },
        }
    }

    fn is_static_context(&self) -> bool {
        match self.owner {
            DefWithBodyId::Method(m) => self.tree.method(m).modifiers.raw & Modifiers::STATIC != 0,
            DefWithBodyId::Constructor(_) => false,
            DefWithBodyId::Initializer(i) => self.tree.initializer(i).is_static,
        }
    }

    fn enclosing_class_type(&self, loader: &mut ExternalTypeLoader<'_>) -> Option<Type> {
        let mut scope = Some(self.scope_id);
        while let Some(id) = scope {
            let data = self.scopes.scope(id);
            if let ScopeKind::Class { item } = data.kind() {
                let ty_name = self.scopes.type_name(*item)?;
                let class_id = loader.store.intern_class_id(ty_name.as_str());
                return Some(Type::class(class_id, Vec::new()));
            }

            scope = data.parent();
        }

        None
    }

    fn infer_binary(
        &mut self,
        loader: &mut ExternalTypeLoader<'_>,
        op: BinaryOp,
        lhs: HirExprId,
        rhs: HirExprId,
    ) -> ExprInfo {
        let lhs_ty = self.infer_expr(loader, lhs).ty;
        let rhs_ty = self.infer_expr(loader, rhs).ty;

        let env_ro: &dyn TypeEnv = &*loader.store;
        let string_ty = Type::class(loader.store.well_known().string, vec![]);
        if op == BinaryOp::Add && (lhs_ty == string_ty || rhs_ty == string_ty) {
            return ExprInfo {
                ty: string_ty,
                is_type_ref: false,
            };
        }

        match (&lhs_ty, &rhs_ty) {
            (Type::Primitive(a), Type::Primitive(b)) => {
                if let Some(result) = binary_numeric_promotion(*a, *b) {
                    return ExprInfo {
                        ty: Type::Primitive(result),
                        is_type_ref: false,
                    };
                }
            }
            _ => {}
        }

        // Best-effort fallback: if both operands are reference types and we're adding, assume string
        // concatenation (e.g. `"" + obj`).
        if op == BinaryOp::Add && (lhs_ty.is_reference() || rhs_ty.is_reference()) {
            return ExprInfo {
                ty: string_ty,
                is_type_ref: false,
            };
        }

        let _ = env_ro;
        ExprInfo {
            ty: Type::Unknown,
            is_type_ref: false,
        }
    }

    fn resolve_source_type(
        &mut self,
        loader: &mut ExternalTypeLoader<'_>,
        text: &str,
        base_span: Option<Span>,
    ) -> Type {
        preload_type_names(self.resolver, self.scopes, self.scope_id, loader, text);
        let vars: HashMap<String, TypeVarId> = HashMap::new();
        let resolved = nova_resolve::type_ref::resolve_type_ref_text(
            self.resolver,
            self.scopes,
            self.scope_id,
            &*loader.store,
            &vars,
            text,
            base_span,
        );
        for diag in resolved.diagnostics {
            self.diagnostics.push(diag);
        }
        resolved.ty
    }

    fn tick(&mut self) {
        cancel::checkpoint_cancelled_every(self.db, self.steps, 256);
        self.steps = self.steps.wrapping_add(1);
    }
}

fn params_for_owner(tree: &nova_hir::item_tree::ItemTree, owner: DefWithBodyId) -> Vec<ParamId> {
    match owner {
        DefWithBodyId::Method(m) => tree
            .method(m)
            .params
            .iter()
            .enumerate()
            .map(|(idx, _)| ParamId::new(owner, idx as u32))
            .collect(),
        DefWithBodyId::Constructor(c) => tree
            .constructor(c)
            .params
            .iter()
            .enumerate()
            .map(|(idx, _)| ParamId::new(owner, idx as u32))
            .collect(),
        DefWithBodyId::Initializer(_) => Vec::new(),
    }
}

fn resolve_expected_return_type<'idx>(
    resolver: &nova_resolve::Resolver<'idx>,
    scopes: &nova_resolve::ScopeGraph,
    scope_id: nova_resolve::ScopeId,
    tree: &nova_hir::item_tree::ItemTree,
    owner: DefWithBodyId,
    loader: &mut ExternalTypeLoader<'_>,
) -> (Type, Vec<Diagnostic>) {
    match owner {
        DefWithBodyId::Method(m) => {
            let method = tree.method(m);
            let resolved =
                resolve_type_ref_text(resolver, scopes, scope_id, loader, &method.return_ty, None);
            (resolved.ty, resolved.diagnostics)
        }
        DefWithBodyId::Constructor(_) | DefWithBodyId::Initializer(_) => (Type::Void, Vec::new()),
    }
}

fn resolve_param_types<'idx>(
    resolver: &nova_resolve::Resolver<'idx>,
    scopes: &nova_resolve::ScopeGraph,
    scope_id: nova_resolve::ScopeId,
    tree: &nova_hir::item_tree::ItemTree,
    owner: DefWithBodyId,
    loader: &mut ExternalTypeLoader<'_>,
) -> (Vec<Type>, Vec<Diagnostic>) {
    let mut out = Vec::new();
    let mut diags = Vec::new();

    let params = match owner {
        DefWithBodyId::Method(m) => tree.method(m).params.as_slice(),
        DefWithBodyId::Constructor(c) => tree.constructor(c).params.as_slice(),
        DefWithBodyId::Initializer(_) => &[],
    };

    for param in params {
        let resolved = resolve_type_ref_text(resolver, scopes, scope_id, loader, &param.ty, None);
        diags.extend(resolved.diagnostics);
        out.push(resolved.ty);
    }

    (out, diags)
}

fn resolve_type_ref_text<'idx>(
    resolver: &nova_resolve::Resolver<'idx>,
    scopes: &nova_resolve::ScopeGraph,
    scope_id: nova_resolve::ScopeId,
    loader: &mut ExternalTypeLoader<'_>,
    text: &str,
    base_span: Option<Span>,
) -> nova_resolve::type_ref::ResolvedType {
    preload_type_names(resolver, scopes, scope_id, loader, text);
    let vars: HashMap<String, TypeVarId> = HashMap::new();
    nova_resolve::type_ref::resolve_type_ref_text(
        resolver,
        scopes,
        scope_id,
        &*loader.store,
        &vars,
        text,
        base_span,
    )
}

fn param_name_lookup(tree: &nova_hir::item_tree::ItemTree, id: ParamId) -> Name {
    match id.owner {
        DefWithBodyId::Method(m) => tree
            .method(m)
            .params
            .get(id.index as usize)
            .map(|p| Name::from(p.name.as_str()))
            .unwrap_or_else(|| Name::from("<param>")),
        DefWithBodyId::Constructor(c) => tree
            .constructor(c)
            .params
            .get(id.index as usize)
            .map(|p| Name::from(p.name.as_str()))
            .unwrap_or_else(|| Name::from("<param>")),
        DefWithBodyId::Initializer(_) => Name::from("<param>"),
    }
}

fn define_source_types<'idx>(
    resolver: &nova_resolve::Resolver<'idx>,
    scopes: &nova_resolve::ItemTreeScopeBuildResult,
    tree: &nova_hir::item_tree::ItemTree,
    loader: &mut ExternalTypeLoader<'_>,
) -> (HashMap<FieldId, Type>, HashMap<MethodId, (Vec<Type>, Type)>) {
    let mut items = Vec::new();
    for item in &tree.items {
        collect_item_ids(tree, *item, &mut items);
    }

    // First pass: intern ids.
    for item in &items {
        if let Some(name) = scopes.scopes.type_name(*item) {
            loader.store.intern_class_id(name.as_str());
        }
    }

    let mut field_types = HashMap::new();
    let mut method_types = HashMap::new();

    // Second pass: define skeleton class defs.
    for item in items {
        let Some(name) = scopes
            .scopes
            .type_name(item)
            .map(|t| t.as_str().to_string())
        else {
            continue;
        };

        let class_id = loader.store.intern_class_id(&name);
        let kind = match item {
            nova_hir::ids::ItemId::Interface(_) => ClassKind::Interface,
            _ => ClassKind::Class,
        };

        let object_ty = Type::class(loader.store.well_known().object, vec![]);
        let super_class = if name == "java.lang.Object" {
            None
        } else {
            Some(object_ty.clone())
        };

        let class_scope = scopes
            .class_scopes
            .get(&item)
            .copied()
            .unwrap_or(scopes.file_scope);

        let mut fields = Vec::new();
        let mut methods = Vec::new();
        for member in item_members(tree, item) {
            match member {
                nova_hir::item_tree::Member::Field(fid) => {
                    let field = tree.field(*fid);
                    preload_type_names(resolver, &scopes.scopes, class_scope, loader, &field.ty);
                    let vars: HashMap<String, TypeVarId> = HashMap::new();
                    let ty = nova_resolve::type_ref::resolve_type_ref_text(
                        resolver,
                        &scopes.scopes,
                        class_scope,
                        &*loader.store,
                        &vars,
                        &field.ty,
                        None,
                    )
                    .ty;
                    field_types.insert(*fid, ty.clone());
                    let is_static =
                        field.modifiers.raw & nova_hir::item_tree::Modifiers::STATIC != 0;
                    let is_final =
                        field.modifiers.raw & nova_hir::item_tree::Modifiers::FINAL != 0;
                    fields.push(FieldDef {
                        name: field.name.clone(),
                        ty,
                        is_static,
                        is_final,
                    });
                }
                nova_hir::item_tree::Member::Method(mid) => {
                    let method = tree.method(*mid);
                    let scope = scopes
                        .method_scopes
                        .get(mid)
                        .copied()
                        .unwrap_or(class_scope);
                    let vars: HashMap<String, TypeVarId> = HashMap::new();

                    let params = method
                        .params
                        .iter()
                        .map(|p| {
                            preload_type_names(resolver, &scopes.scopes, scope, loader, &p.ty);
                            nova_resolve::type_ref::resolve_type_ref_text(
                                resolver,
                                &scopes.scopes,
                                scope,
                                &*loader.store,
                                &vars,
                                &p.ty,
                                None,
                            )
                            .ty
                        })
                        .collect::<Vec<_>>();

                    preload_type_names(resolver, &scopes.scopes, scope, loader, &method.return_ty);
                    let return_type = nova_resolve::type_ref::resolve_type_ref_text(
                        resolver,
                        &scopes.scopes,
                        scope,
                        &*loader.store,
                        &vars,
                        &method.return_ty,
                        None,
                    )
                    .ty;
                    method_types.insert(*mid, (params.clone(), return_type.clone()));
                    let is_static =
                        method.modifiers.raw & nova_hir::item_tree::Modifiers::STATIC != 0;

                    methods.push(MethodDef {
                        name: method.name.clone(),
                        type_params: Vec::new(),
                        params,
                        return_type,
                        is_static,
                        is_varargs: false,
                        is_abstract: method.body.is_none(),
                    });
                }
                _ => {}
            }
        }

        loader.store.define_class(
            class_id,
            ClassDef {
                name,
                kind,
                type_params: Vec::new(),
                super_class,
                interfaces: Vec::new(),
                fields,
                constructors: Vec::new(),
                methods,
            },
        );
    }

    (field_types, method_types)
}

fn item_members<'a>(
    tree: &'a nova_hir::item_tree::ItemTree,
    item: nova_hir::ids::ItemId,
) -> &'a [nova_hir::item_tree::Member] {
    match item {
        nova_hir::ids::ItemId::Class(id) => &tree.class(id).members,
        nova_hir::ids::ItemId::Interface(id) => &tree.interface(id).members,
        nova_hir::ids::ItemId::Enum(id) => &tree.enum_(id).members,
        nova_hir::ids::ItemId::Record(id) => &tree.record(id).members,
        nova_hir::ids::ItemId::Annotation(id) => &tree.annotation(id).members,
    }
}

fn collect_item_ids(
    tree: &nova_hir::item_tree::ItemTree,
    item: nova_hir::item_tree::Item,
    out: &mut Vec<nova_hir::ids::ItemId>,
) {
    let id = match item {
        nova_hir::item_tree::Item::Class(id) => nova_hir::ids::ItemId::Class(id),
        nova_hir::item_tree::Item::Interface(id) => nova_hir::ids::ItemId::Interface(id),
        nova_hir::item_tree::Item::Enum(id) => nova_hir::ids::ItemId::Enum(id),
        nova_hir::item_tree::Item::Record(id) => nova_hir::ids::ItemId::Record(id),
        nova_hir::item_tree::Item::Annotation(id) => nova_hir::ids::ItemId::Annotation(id),
    };
    out.push(id);
    for member in item_members(tree, id) {
        if let nova_hir::item_tree::Member::Type(child) = member {
            collect_item_ids(tree, *child, out);
        }
    }
}

fn collect_body_owners(tree: &nova_hir::item_tree::ItemTree) -> Vec<DefWithBodyId> {
    let mut owners = Vec::new();
    for item in &tree.items {
        collect_body_owners_in_item(tree, *item, &mut owners);
    }
    owners
}

fn collect_body_owners_in_item(
    tree: &nova_hir::item_tree::ItemTree,
    item: nova_hir::item_tree::Item,
    out: &mut Vec<DefWithBodyId>,
) {
    let id = match item {
        nova_hir::item_tree::Item::Class(id) => nova_hir::ids::ItemId::Class(id),
        nova_hir::item_tree::Item::Interface(id) => nova_hir::ids::ItemId::Interface(id),
        nova_hir::item_tree::Item::Enum(id) => nova_hir::ids::ItemId::Enum(id),
        nova_hir::item_tree::Item::Record(id) => nova_hir::ids::ItemId::Record(id),
        nova_hir::item_tree::Item::Annotation(id) => nova_hir::ids::ItemId::Annotation(id),
    };

    for member in item_members(tree, id) {
        match *member {
            nova_hir::item_tree::Member::Method(m) => {
                if tree.method(m).body.is_some() {
                    out.push(DefWithBodyId::Method(m));
                }
            }
            nova_hir::item_tree::Member::Constructor(c) => out.push(DefWithBodyId::Constructor(c)),
            nova_hir::item_tree::Member::Initializer(i) => out.push(DefWithBodyId::Initializer(i)),
            nova_hir::item_tree::Member::Type(child) => {
                collect_body_owners_in_item(tree, child, out)
            }
            nova_hir::item_tree::Member::Field(_) => {}
        }
    }
}

fn def_file(def: DefWithBodyId) -> FileId {
    match def {
        DefWithBodyId::Method(m) => m.file,
        DefWithBodyId::Constructor(c) => c.file,
        DefWithBodyId::Initializer(i) => i.file,
    }
}

fn find_best_expr_in_stmt(
    body: &HirBody,
    stmt: nova_hir::hir::StmtId,
    offset: usize,
    owner: DefWithBodyId,
    best: &mut Option<(DefWithBodyId, HirExprId, usize)>,
) {
    match &body.stmts[stmt] {
        HirStmt::Block { statements, .. } => {
            for &stmt in statements {
                find_best_expr_in_stmt(body, stmt, offset, owner, best);
            }
        }
        HirStmt::Let { initializer, .. } => {
            if let Some(expr) = initializer {
                find_best_expr_in_expr(body, *expr, offset, owner, best);
            }
        }
        HirStmt::Expr { expr, .. } => find_best_expr_in_expr(body, *expr, offset, owner, best),
        HirStmt::Return { expr, .. } => {
            if let Some(expr) = expr {
                find_best_expr_in_expr(body, *expr, offset, owner, best);
            }
        }
        HirStmt::If {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            find_best_expr_in_expr(body, *condition, offset, owner, best);
            find_best_expr_in_stmt(body, *then_branch, offset, owner, best);
            if let Some(else_branch) = else_branch {
                find_best_expr_in_stmt(body, *else_branch, offset, owner, best);
            }
        }
        HirStmt::While {
            condition,
            body: loop_body,
            ..
        } => {
            find_best_expr_in_expr(body, *condition, offset, owner, best);
            find_best_expr_in_stmt(body, *loop_body, offset, owner, best);
        }
        HirStmt::For {
            init,
            condition,
            update,
            body: for_body,
            ..
        } => {
            for stmt in init {
                find_best_expr_in_stmt(body, *stmt, offset, owner, best);
            }
            if let Some(condition) = condition {
                find_best_expr_in_expr(body, *condition, offset, owner, best);
            }
            for expr in update {
                find_best_expr_in_expr(body, *expr, offset, owner, best);
            }
            find_best_expr_in_stmt(body, *for_body, offset, owner, best);
        }
        HirStmt::ForEach {
            iterable,
            body: foreach_body,
            ..
        } => {
            find_best_expr_in_expr(body, *iterable, offset, owner, best);
            find_best_expr_in_stmt(body, *foreach_body, offset, owner, best);
        }
        HirStmt::Switch {
            selector,
            body: switch_body,
            ..
        } => {
            find_best_expr_in_expr(body, *selector, offset, owner, best);
            find_best_expr_in_stmt(body, *switch_body, offset, owner, best);
        }
        HirStmt::Try {
            body: try_body,
            catches,
            finally,
            ..
        } => {
            find_best_expr_in_stmt(body, *try_body, offset, owner, best);
            for catch in catches {
                find_best_expr_in_stmt(body, catch.body, offset, owner, best);
            }
            if let Some(finally) = finally {
                find_best_expr_in_stmt(body, *finally, offset, owner, best);
            }
        }
        HirStmt::Throw { expr, .. } => find_best_expr_in_expr(body, *expr, offset, owner, best),
        HirStmt::Break { .. } | HirStmt::Continue { .. } => {}
        HirStmt::Empty { .. } => {}
    }
}

fn find_best_expr_in_expr(
    body: &HirBody,
    expr: HirExprId,
    offset: usize,
    owner: DefWithBodyId,
    best: &mut Option<(DefWithBodyId, HirExprId, usize)>,
) {
    let range = body.exprs[expr].range();
    // `Span` uses end-exclusive semantics (mirrors `text_size::TextRange`).
    if range.start <= offset && offset < range.end {
        let len = range.len();
        let replace = best.map(|(_, _, best_len)| len < best_len).unwrap_or(true);
        if replace {
            *best = Some((owner, expr, len));
        }
    }

    match &body.exprs[expr] {
        HirExpr::FieldAccess { receiver, .. } => {
            find_best_expr_in_expr(body, *receiver, offset, owner, best);
        }
        HirExpr::MethodReference { receiver, .. } => {
            find_best_expr_in_expr(body, *receiver, offset, owner, best);
        }
        HirExpr::ConstructorReference { receiver, .. } => {
            find_best_expr_in_expr(body, *receiver, offset, owner, best);
        }
        HirExpr::ClassLiteral { ty, .. } => {
            find_best_expr_in_expr(body, *ty, offset, owner, best);
        }
        HirExpr::Call { callee, args, .. } => {
            find_best_expr_in_expr(body, *callee, offset, owner, best);
            for arg in args {
                find_best_expr_in_expr(body, *arg, offset, owner, best);
            }
        }
        HirExpr::New { args, .. } => {
            for arg in args {
                find_best_expr_in_expr(body, *arg, offset, owner, best);
            }
        }
        HirExpr::Unary { expr, .. } => find_best_expr_in_expr(body, *expr, offset, owner, best),
        HirExpr::Binary { lhs, rhs, .. } => {
            find_best_expr_in_expr(body, *lhs, offset, owner, best);
            find_best_expr_in_expr(body, *rhs, offset, owner, best);
        }
        HirExpr::Assign { lhs, rhs, .. } => {
            find_best_expr_in_expr(body, *lhs, offset, owner, best);
            find_best_expr_in_expr(body, *rhs, offset, owner, best);
        }
        HirExpr::Conditional {
            condition,
            then_expr,
            else_expr,
            ..
        } => {
            find_best_expr_in_expr(body, *condition, offset, owner, best);
            find_best_expr_in_expr(body, *then_expr, offset, owner, best);
            find_best_expr_in_expr(body, *else_expr, offset, owner, best);
        }
        HirExpr::Lambda {
            body: lambda_body, ..
        } => match lambda_body {
            LambdaBody::Expr(expr) => find_best_expr_in_expr(body, *expr, offset, owner, best),
            LambdaBody::Block(stmt) => find_best_expr_in_stmt(body, *stmt, offset, owner, best),
        },
        HirExpr::Name { .. }
        | HirExpr::Literal { .. }
        | HirExpr::Null { .. }
        | HirExpr::This { .. }
        | HirExpr::Super { .. }
        | HirExpr::Missing { .. } => {}
    }
}

fn preload_type_names<'idx>(
    resolver: &nova_resolve::Resolver<'idx>,
    scopes: &nova_resolve::ScopeGraph,
    scope_id: nova_resolve::ScopeId,
    loader: &mut ExternalTypeLoader<'_>,
    text: &str,
) {
    let mut i = 0usize;
    let bytes = text.as_bytes();

    while i < bytes.len() {
        let b = bytes[i];
        if !is_ident_start(b) {
            i += 1;
            continue;
        }

        let start = i;
        i += 1;
        while i < bytes.len() && is_ident_continue(bytes[i]) {
            i += 1;
        }

        let mut end = i;
        while end < bytes.len() && bytes[end] == b'.' {
            let seg_start = end + 1;
            if seg_start >= bytes.len() || !is_ident_start(bytes[seg_start]) {
                break;
            }
            end = seg_start + 1;
            while end < bytes.len() && is_ident_continue(bytes[end]) {
                end += 1;
            }
        }

        let candidate = &text[start..end];
        i = end;

        if is_primitive_or_keyword(candidate) {
            continue;
        }

        let q = QualifiedName::from_dotted(candidate);
        let Some(resolved) = resolver.resolve_qualified_type_in_scope(scopes, scope_id, &q) else {
            continue;
        };
        let _ = loader.ensure_class(resolved.as_str());
    }
}

fn is_ident_start(b: u8) -> bool {
    matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'_' | b'$')
}

fn is_ident_continue(b: u8) -> bool {
    is_ident_start(b) || matches!(b, b'0'..=b'9')
}

fn is_primitive_or_keyword(word: &str) -> bool {
    matches!(
        word,
        "boolean"
            | "byte"
            | "short"
            | "int"
            | "long"
            | "char"
            | "float"
            | "double"
            | "void"
            | "extends"
            | "super"
            | "var"
    )
}

fn ensure_type_loaded(loader: &mut ExternalTypeLoader<'_>, ty: &Type) {
    match ty {
        Type::Class(nova_types::ClassType { def, .. }) => {
            let Some(name) = loader.store.class(*def).map(|def| def.name.clone()) else {
                return;
            };
            let _ = loader.ensure_class(&name);
        }
        Type::Named(name) => {
            let _ = loader.ensure_class(name);
        }
        _ => {}
    }
}

fn type_binary_name(env: &TypeStore, ty: &Type) -> Option<String> {
    match ty {
        Type::Class(nova_types::ClassType { def, .. }) => env.class(*def).map(|c| c.name.clone()),
        Type::Named(name) => Some(name.clone()),
        _ => None,
    }
}
