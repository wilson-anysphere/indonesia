//! Expression and statement scope mapping for Java bodies.
//!
//! `ExprScopes` is a lightweight, body-only view of lexical scoping. It mirrors the
//! same order-sensitive Java rules as the file-wide scope graph builder:
//! - A local variable is only in scope after its declaration.
//! - (Unlike Rust) a local *is* in scope within its own initializer.
//!
//! `ExprScopes` provides a query-friendly representation of the lexical scopes
//! inside a single body (`nova_hir::hir::Body`), plus a mapping from each
//! expression / statement ID to the scope active at that node.

use std::collections::HashMap;

use nova_core::Name;
use nova_hir::hir::{Body, Expr, ExprId, LocalId, Stmt, StmtId};

use crate::ids::ParamId;

/// Identifier for a lexical scope within a body.
///
/// This is an index into [`ExprScopes::scopes`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ScopeId(u32);

impl ScopeId {
    #[must_use]
    pub const fn idx(self) -> usize {
        self.0 as usize
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedValue {
    Local(LocalId),
    Param(ParamId),
}

#[derive(Debug, Clone, Default)]
pub struct ScopeData {
    parent: Option<ScopeId>,
    entries: HashMap<Name, ResolvedValue>,
}

impl ScopeData {
    #[must_use]
    pub fn parent(&self) -> Option<ScopeId> {
        self.parent
    }

    #[must_use]
    pub fn entries(&self) -> &HashMap<Name, ResolvedValue> {
        &self.entries
    }
}

#[derive(Debug, Clone)]
pub struct ExprScopes {
    scopes: Vec<ScopeData>,
    expr_scopes: HashMap<ExprId, ScopeId>,
    stmt_scopes: HashMap<StmtId, ScopeId>,
    root_scope: ScopeId,
}

impl ExprScopes {
    /// Build expression scopes for a single body.
    ///
    /// `params` is the list of parameters in scope at the body root.
    pub fn new(
        body: &Body,
        params: &[ParamId],
        param_names: impl Fn(ParamId) -> Name,
    ) -> ExprScopes {
        let mut scopes = Vec::<ScopeData>::new();

        let mut root_entries = HashMap::new();
        for &param in params {
            root_entries.insert(param_names(param), ResolvedValue::Param(param));
        }

        let root_scope = ScopeId(scopes.len() as u32);
        scopes.push(ScopeData {
            parent: None,
            entries: root_entries,
        });

        let mut builder = Builder {
            scopes,
            expr_scopes: HashMap::new(),
            stmt_scopes: HashMap::new(),
        };

        builder.visit_stmt(body, body.root, root_scope);

        ExprScopes {
            scopes: builder.scopes,
            expr_scopes: builder.expr_scopes,
            stmt_scopes: builder.stmt_scopes,
            root_scope,
        }
    }

    #[must_use]
    pub fn root_scope(&self) -> ScopeId {
        self.root_scope
    }

    #[must_use]
    pub fn scope_data(&self, scope: ScopeId) -> &ScopeData {
        &self.scopes[scope.idx()]
    }

    #[must_use]
    pub fn scope_for_expr(&self, expr: ExprId) -> Option<ScopeId> {
        self.expr_scopes.get(&expr).copied()
    }

    #[must_use]
    pub fn scope_for_stmt(&self, stmt: StmtId) -> Option<ScopeId> {
        self.stmt_scopes.get(&stmt).copied()
    }

    /// Resolve a simple name by walking up the scope parent chain.
    #[must_use]
    pub fn resolve_name(&self, scope: ScopeId, name: &Name) -> Option<ResolvedValue> {
        let mut current = Some(scope);
        while let Some(id) = current {
            let data = &self.scopes[id.idx()];
            if let Some(&value) = data.entries.get(name) {
                return Some(value);
            }
            current = data.parent;
        }
        None
    }
}

struct Builder {
    scopes: Vec<ScopeData>,
    expr_scopes: HashMap<ExprId, ScopeId>,
    stmt_scopes: HashMap<StmtId, ScopeId>,
}

impl Builder {
    fn alloc_scope(&mut self, parent: Option<ScopeId>) -> ScopeId {
        let id = ScopeId(self.scopes.len() as u32);
        self.scopes.push(ScopeData {
            parent,
            entries: HashMap::new(),
        });
        id
    }

    fn visit_stmt(&mut self, body: &Body, stmt_id: StmtId, scope: ScopeId) -> ScopeId {
        match &body.stmts[stmt_id] {
            Stmt::Block { statements, .. } => {
                let block_scope = self.alloc_scope(Some(scope));
                self.stmt_scopes.insert(stmt_id, block_scope);

                let mut current = block_scope;
                for &stmt in statements {
                    current = self.visit_stmt(body, stmt, current);
                }

                // A nested block doesn't introduce bindings in the parent scope.
                scope
            }
            Stmt::Let {
                local, initializer, ..
            } => {
                // Java: local is in scope within its initializer.
                let let_scope = self.alloc_scope(Some(scope));
                let local_data = &body.locals[*local];
                let name = Name::from(local_data.name.as_str());
                self.scopes[let_scope.idx()]
                    .entries
                    .insert(name, ResolvedValue::Local(*local));

                self.stmt_scopes.insert(stmt_id, let_scope);

                if let Some(expr) = initializer {
                    self.visit_expr(body, *expr, let_scope);
                }

                // Following statements in the block see the new binding.
                let_scope
            }
            Stmt::Expr { expr, .. } => {
                self.stmt_scopes.insert(stmt_id, scope);
                self.visit_expr(body, *expr, scope);
                scope
            }
            Stmt::Return { expr, .. } => {
                self.stmt_scopes.insert(stmt_id, scope);
                if let Some(expr) = expr {
                    self.visit_expr(body, *expr, scope);
                }
                scope
            }
            Stmt::If {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                self.stmt_scopes.insert(stmt_id, scope);
                self.visit_expr(body, *condition, scope);

                let then_scope = self.alloc_scope(Some(scope));
                let _ = self.visit_stmt(body, *then_branch, then_scope);

                if let Some(stmt) = else_branch {
                    let else_scope = self.alloc_scope(Some(scope));
                    let _ = self.visit_stmt(body, *stmt, else_scope);
                }

                scope
            }
            Stmt::While {
                condition, body: b, ..
            } => {
                self.stmt_scopes.insert(stmt_id, scope);
                self.visit_expr(body, *condition, scope);
                let body_scope = self.alloc_scope(Some(scope));
                let _ = self.visit_stmt(body, *b, body_scope);
                scope
            }
            Stmt::For {
                init,
                condition,
                update,
                body: b,
                ..
            } => {
                let for_scope = self.alloc_scope(Some(scope));
                self.stmt_scopes.insert(stmt_id, for_scope);

                let mut current = for_scope;
                for stmt in init {
                    current = self.visit_stmt(body, *stmt, current);
                }

                if let Some(expr) = condition {
                    self.visit_expr(body, *expr, current);
                }
                for expr in update {
                    self.visit_expr(body, *expr, current);
                }

                let _ = self.visit_stmt(body, *b, current);
                scope
            }
            Stmt::ForEach {
                local,
                iterable,
                body: b,
                ..
            } => {
                self.stmt_scopes.insert(stmt_id, scope);
                self.visit_expr(body, *iterable, scope);

                let for_scope = self.alloc_scope(Some(scope));
                let local_data = &body.locals[*local];
                self.scopes[for_scope.idx()].entries.insert(
                    Name::from(local_data.name.as_str()),
                    ResolvedValue::Local(*local),
                );

                let _ = self.visit_stmt(body, *b, for_scope);
                scope
            }
            Stmt::Switch {
                selector, body: b, ..
            } => {
                self.stmt_scopes.insert(stmt_id, scope);
                self.visit_expr(body, *selector, scope);
                let switch_scope = self.alloc_scope(Some(scope));
                let _ = self.visit_stmt(body, *b, switch_scope);
                scope
            }
            Stmt::Try {
                body: b,
                catches,
                finally,
                ..
            } => {
                self.stmt_scopes.insert(stmt_id, scope);

                let _ = self.visit_stmt(body, *b, scope);
                for catch in catches {
                    let catch_scope = self.alloc_scope(Some(scope));
                    let local_data = &body.locals[catch.param];
                    self.scopes[catch_scope.idx()].entries.insert(
                        Name::from(local_data.name.as_str()),
                        ResolvedValue::Local(catch.param),
                    );
                    let _ = self.visit_stmt(body, catch.body, catch_scope);
                }

                if let Some(stmt) = finally {
                    let _ = self.visit_stmt(body, *stmt, scope);
                }

                scope
            }
            Stmt::Throw { expr, .. } => {
                self.stmt_scopes.insert(stmt_id, scope);
                self.visit_expr(body, *expr, scope);
                scope
            }
            Stmt::Break { .. } | Stmt::Continue { .. } => {
                self.stmt_scopes.insert(stmt_id, scope);
                scope
            }
            Stmt::Empty { .. } => {
                self.stmt_scopes.insert(stmt_id, scope);
                scope
            }
        }
    }

    fn visit_expr(&mut self, body: &Body, expr_id: ExprId, scope: ScopeId) {
        self.expr_scopes.insert(expr_id, scope);

        match &body.exprs[expr_id] {
            Expr::Name { .. }
            | Expr::Literal { .. }
            | Expr::Null { .. }
            | Expr::This { .. }
            | Expr::Super { .. }
            | Expr::Missing { .. } => {}
            Expr::FieldAccess { receiver, .. } => {
                self.visit_expr(body, *receiver, scope);
            }
            Expr::ArrayAccess { array, index, .. } => {
                self.visit_expr(body, *array, scope);
                self.visit_expr(body, *index, scope);
            }
            Expr::MethodReference { receiver, .. }
            | Expr::ConstructorReference { receiver, .. } => {
                self.visit_expr(body, *receiver, scope);
            }
            Expr::ClassLiteral { ty, .. } => {
                self.visit_expr(body, *ty, scope);
            }
            Expr::Call { callee, args, .. } => {
                self.visit_expr(body, *callee, scope);
                for &arg in args {
                    self.visit_expr(body, arg, scope);
                }
            }
            Expr::New { args, .. } => {
                for &arg in args {
                    self.visit_expr(body, arg, scope);
                }
            }
            Expr::Unary { expr, .. } => {
                self.visit_expr(body, *expr, scope);
            }
            Expr::Binary { lhs, rhs, .. } => {
                self.visit_expr(body, *lhs, scope);
                self.visit_expr(body, *rhs, scope);
            }
            Expr::Instanceof { expr, .. } => {
                self.visit_expr(body, *expr, scope);
            }
            Expr::Assign { lhs, rhs, .. } => {
                self.visit_expr(body, *lhs, scope);
                self.visit_expr(body, *rhs, scope);
            }
            Expr::Conditional {
                condition,
                then_expr,
                else_expr,
                ..
            } => {
                self.visit_expr(body, *condition, scope);
                self.visit_expr(body, *then_expr, scope);
                self.visit_expr(body, *else_expr, scope);
            }
            Expr::Lambda {
                params, body: b, ..
            } => {
                let lambda_scope = self.alloc_scope(Some(scope));

                for param in params {
                    let local = param.local;
                    let local_data = &body.locals[local];
                    self.scopes[lambda_scope.idx()].entries.insert(
                        Name::from(local_data.name.as_str()),
                        ResolvedValue::Local(local),
                    );
                }

                match b {
                    nova_hir::hir::LambdaBody::Expr(expr) => {
                        self.visit_expr(body, *expr, lambda_scope)
                    }
                    nova_hir::hir::LambdaBody::Block(stmt) => {
                        let _ = self.visit_stmt(body, *stmt, lambda_scope);
                    }
                }
            }
            Expr::Invalid { children, .. } => {
                for &child in children {
                    self.visit_expr(body, child, scope);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use nova_hir::lowering::lower_body;
    use nova_syntax::java::parse_block;

    fn build_scopes(block: &str, params: &[(&str, ParamId)]) -> (Body, ExprScopes) {
        let block = parse_block(block, 0);
        let body = lower_body(&block);

        let param_ids: Vec<_> = params.iter().map(|(_, id)| *id).collect();
        let scopes = ExprScopes::new(&body, &param_ids, |id| {
            params
                .iter()
                .find_map(|(name, pid)| (*pid == id).then(|| Name::from(*name)))
                .unwrap()
        });

        (body, scopes)
    }

    fn root_statements(body: &Body) -> &[StmtId] {
        match &body.stmts[body.root] {
            Stmt::Block { statements, .. } => statements,
            other => panic!("expected root block, got {other:?}"),
        }
    }

    #[test]
    fn let_ordering_allows_previous_bindings() {
        let (body, scopes) = build_scopes("{ int a = 0; int b = a; }", &[]);
        let stmts = root_statements(&body);
        let stmt_a = stmts[0];
        let stmt_b = stmts[1];

        let local_a = match &body.stmts[stmt_a] {
            Stmt::Let { local, .. } => *local,
            other => panic!("expected let, got {other:?}"),
        };

        let a_in_b_initializer = match &body.stmts[stmt_b] {
            Stmt::Let {
                initializer: Some(expr),
                ..
            } => *expr,
            other => panic!("expected let with initializer, got {other:?}"),
        };

        let scope = scopes.scope_for_expr(a_in_b_initializer).unwrap();
        assert_eq!(
            scopes.resolve_name(scope, &Name::from("a")),
            Some(ResolvedValue::Local(local_a))
        );
    }

    #[test]
    fn let_ordering_does_not_allow_future_bindings() {
        let (body, scopes) = build_scopes("{ System.out.println(a); int a = 0; }", &[]);
        let stmts = root_statements(&body);
        let stmt_print = stmts[0];

        let call_expr = match &body.stmts[stmt_print] {
            Stmt::Expr { expr, .. } => *expr,
            other => panic!("expected expr stmt, got {other:?}"),
        };

        let a_expr = match &body.exprs[call_expr] {
            Expr::Call { args, .. } => args[0],
            other => panic!("expected call expr, got {other:?}"),
        };

        let scope = scopes.scope_for_expr(a_expr).unwrap();
        assert_eq!(scopes.resolve_name(scope, &Name::from("a")), None);
    }

    #[test]
    fn shadowing_resolves_to_innermost_binding() {
        let (body, scopes) = build_scopes("{ int x = 0; { int x = 1; x; } x; }", &[]);
        let stmts = root_statements(&body);
        let stmt_outer_let = stmts[0];
        let stmt_inner_block = stmts[1];
        let stmt_outer_use = stmts[2];

        let local_outer = match &body.stmts[stmt_outer_let] {
            Stmt::Let { local, .. } => *local,
            other => panic!("expected let, got {other:?}"),
        };

        let outer_use_expr = match &body.stmts[stmt_outer_use] {
            Stmt::Expr { expr, .. } => *expr,
            other => panic!("expected expr stmt, got {other:?}"),
        };

        let inner_stmts = match &body.stmts[stmt_inner_block] {
            Stmt::Block { statements, .. } => statements,
            other => panic!("expected block stmt, got {other:?}"),
        };
        let stmt_inner_let = inner_stmts[0];
        let stmt_inner_use = inner_stmts[1];

        let local_inner = match &body.stmts[stmt_inner_let] {
            Stmt::Let { local, .. } => *local,
            other => panic!("expected let, got {other:?}"),
        };

        let inner_use_expr = match &body.stmts[stmt_inner_use] {
            Stmt::Expr { expr, .. } => *expr,
            other => panic!("expected expr stmt, got {other:?}"),
        };

        let inner_scope = scopes.scope_for_expr(inner_use_expr).unwrap();
        assert_eq!(
            scopes.resolve_name(inner_scope, &Name::from("x")),
            Some(ResolvedValue::Local(local_inner))
        );

        let outer_scope = scopes.scope_for_expr(outer_use_expr).unwrap();
        assert_eq!(
            scopes.resolve_name(outer_scope, &Name::from("x")),
            Some(ResolvedValue::Local(local_outer))
        );
    }

    #[test]
    fn local_is_in_scope_in_its_initializer() {
        let (body, scopes) = build_scopes("{ int x = x; }", &[]);
        let stmts = root_statements(&body);
        let stmt_x = stmts[0];

        let (local_x, init_expr) = match &body.stmts[stmt_x] {
            Stmt::Let {
                local,
                initializer: Some(expr),
                ..
            } => (*local, *expr),
            other => panic!("expected let with initializer, got {other:?}"),
        };

        let scope = scopes.scope_for_expr(init_expr).unwrap();
        assert_eq!(
            scopes.resolve_name(scope, &Name::from("x")),
            Some(ResolvedValue::Local(local_x))
        );
    }

    #[test]
    fn params_are_visible_in_body_scope() {
        let owner = crate::DefWithBodyId::Method(nova_hir::ids::MethodId::new(
            nova_core::FileId::from_raw(0),
            nova_hir::ast_id::AstId::new(0),
        ));
        let param = ParamId::new(owner, 0);
        let (body, scopes) = build_scopes("{ p; }", &[("p", param)]);
        let stmts = root_statements(&body);
        let stmt_p = stmts[0];
        let expr_p = match &body.stmts[stmt_p] {
            Stmt::Expr { expr, .. } => *expr,
            other => panic!("expected expr stmt, got {other:?}"),
        };

        let scope = scopes.scope_for_expr(expr_p).unwrap();
        assert_eq!(
            scopes.resolve_name(scope, &Name::from("p")),
            Some(ResolvedValue::Param(param))
        );
    }
}
