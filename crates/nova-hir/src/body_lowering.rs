//! Lower Rowan-based Java syntax trees into `nova_hir::body` (flow IR).
//!
//! This lowering is intentionally *best-effort* and designed for IDE usage:
//! it should never panic on incomplete/invalid code, and it prefers producing
//! conservative IR over precise Java semantics.

use std::collections::HashMap;

use nova_core::Name;
use nova_syntax::ast::{self, AstNode};
use nova_syntax::{SyntaxKind, SyntaxNode, SyntaxToken};
use nova_types::Span;

use crate::body::{
    BinaryOp, Body, BodyBuilder, ExprId, ExprKind, LocalId, LocalKind, StmtId, StmtKind, SwitchArm,
    UnaryOp,
};

/// Lower a method-like body (block) into flow IR.
///
/// `params` provides the parameter names (and their spans) that should be
/// considered definitely-assigned on entry.
#[must_use]
pub fn lower_flow_body(block: &ast::Block, params: impl IntoIterator<Item = (Name, Span)>) -> Body {
    lower_flow_body_with(block, params, &mut || {})
}

/// Like [`lower_flow_body`], but allows injecting a periodic cancellation
/// checkpoint callback.
#[must_use]
pub fn lower_flow_body_with(
    block: &ast::Block,
    params: impl IntoIterator<Item = (Name, Span)>,
    check_cancelled: &mut dyn FnMut(),
) -> Body {
    let mut lower = FlowBodyLower::new(check_cancelled);
    for (name, span) in params {
        lower.declare_local(name, LocalKind::Param, span);
    }

    let root = lower.lower_block(block);
    lower.builder.finish(root)
}

struct FlowBodyLower<'a> {
    builder: BodyBuilder,
    scopes: Vec<HashMap<Name, LocalId>>,
    check_cancelled: &'a mut dyn FnMut(),
}

impl<'a> FlowBodyLower<'a> {
    fn new(check_cancelled: &'a mut dyn FnMut()) -> Self {
        Self {
            builder: BodyBuilder::new(),
            scopes: vec![HashMap::new()],
            check_cancelled,
        }
    }

    fn check_cancelled(&mut self) {
        (self.check_cancelled)();
    }

    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
        if self.scopes.is_empty() {
            self.scopes.push(HashMap::new());
        }
    }

    fn declare_local(&mut self, name: Name, kind: LocalKind, span: Span) -> LocalId {
        let id = self.builder.local_with_span(name.clone(), kind, span);
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name, id);
        }
        id
    }

    fn lookup_local(&self, name: &str) -> Option<LocalId> {
        let name = Name::new(name);
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(&name).copied())
    }

    fn lower_block(&mut self, block: &ast::Block) -> StmtId {
        self.check_cancelled();

        self.push_scope();
        let mut stmts = Vec::new();
        for stmt in block.statements() {
            self.check_cancelled();
            stmts.extend(self.lower_statement(stmt));
        }
        self.pop_scope();

        self.builder
            .stmt_with_span(StmtKind::Block(stmts), span_of_node(block.syntax()))
    }

    fn lower_scoped_statement(&mut self, stmt: ast::Statement) -> StmtId {
        self.check_cancelled();
        let span = span_of_node(stmt.syntax());
        self.push_scope();
        let mut stmts = self.lower_statement(stmt);
        self.pop_scope();

        if stmts.len() == 1 {
            return stmts.pop().unwrap();
        }

        self.builder.stmt_with_span(StmtKind::Block(stmts), span)
    }

    fn lower_statement(&mut self, stmt: ast::Statement) -> Vec<StmtId> {
        self.check_cancelled();
        let stmt_span = span_of_node(stmt.syntax());
        match stmt {
            ast::Statement::Block(block) => vec![self.lower_block(&block)],

            ast::Statement::LabeledStatement(labeled) => labeled
                .statement()
                .map_or_else(Vec::new, |inner| self.lower_statement(inner)),

            ast::Statement::IfStatement(if_stmt) => {
                let condition = if_stmt
                    .condition()
                    .map(|expr| self.lower_expr(expr))
                    .unwrap_or_else(|| self.alloc_invalid_expr(span_of_node(if_stmt.syntax())));

                let then_branch = if_stmt
                    .then_branch()
                    .map(|stmt| self.lower_scoped_statement(stmt))
                    .unwrap_or_else(|| {
                        self.builder
                            .stmt_with_span(StmtKind::Nop, span_of_node(if_stmt.syntax()))
                    });

                let else_branch = if_stmt
                    .else_branch()
                    .map(|stmt| self.lower_scoped_statement(stmt));

                vec![self.builder.stmt_with_span(
                    StmtKind::If {
                        condition,
                        then_branch,
                        else_branch,
                    },
                    span_of_node(if_stmt.syntax()),
                )]
            }

            ast::Statement::WhileStatement(while_stmt) => {
                let condition = while_stmt
                    .condition()
                    .map(|expr| self.lower_expr(expr))
                    .unwrap_or_else(|| self.alloc_invalid_expr(span_of_node(while_stmt.syntax())));

                let body = while_stmt
                    .body()
                    .map(|stmt| self.lower_scoped_statement(stmt))
                    .unwrap_or_else(|| {
                        self.builder
                            .stmt_with_span(StmtKind::Nop, span_of_node(while_stmt.syntax()))
                    });

                vec![self.builder.stmt_with_span(
                    StmtKind::While { condition, body },
                    span_of_node(while_stmt.syntax()),
                )]
            }

            ast::Statement::DoWhileStatement(do_stmt) => {
                let body = do_stmt
                    .body()
                    .map(|stmt| self.lower_scoped_statement(stmt))
                    .unwrap_or_else(|| {
                        self.builder
                            .stmt_with_span(StmtKind::Nop, span_of_node(do_stmt.syntax()))
                    });
                let condition = do_stmt
                    .condition()
                    .map(|expr| self.lower_expr(expr))
                    .unwrap_or_else(|| self.alloc_invalid_expr(span_of_node(do_stmt.syntax())));

                vec![self.builder.stmt_with_span(
                    StmtKind::DoWhile { body, condition },
                    span_of_node(do_stmt.syntax()),
                )]
            }

            ast::Statement::ForStatement(for_stmt) => self.lower_for_statement(&for_stmt),

            ast::Statement::SwitchStatement(switch_stmt) => {
                self.lower_switch_statement(&switch_stmt)
            }

            ast::Statement::YieldStatement(yield_stmt) => {
                // Best-effort: treat `yield <expr>;` like `break` that also evaluates the yielded
                // expression. This preserves reachability inside the arm without pretending that
                // `yield` returns from the enclosing method.
                let span = span_of_node(yield_stmt.syntax());
                let mut out = Vec::new();
                if let Some(expr) = yield_stmt.expression() {
                    let expr_id = self.lower_expr(expr);
                    out.push(self.builder.stmt_with_span(StmtKind::Expr(expr_id), span));
                }
                out.push(self.builder.stmt_with_span(StmtKind::Break, span));
                out
            }

            ast::Statement::TryStatement(try_stmt) => self.lower_try_statement(&try_stmt),

            ast::Statement::SynchronizedStatement(sync) => {
                // Best-effort: treat as `{ <expr>; <body> }`.
                let mut stmts = Vec::new();
                if let Some(expr) = sync.expression() {
                    let expr_id = self.lower_expr(expr);
                    stmts.push(
                        self.builder
                            .stmt_with_span(StmtKind::Expr(expr_id), span_of_node(sync.syntax())),
                    );
                }
                if let Some(body) = sync.body() {
                    stmts.push(self.lower_block(&body));
                }
                if stmts.is_empty() {
                    stmts.push(
                        self.builder
                            .stmt_with_span(StmtKind::Nop, span_of_node(sync.syntax())),
                    );
                }
                vec![self
                    .builder
                    .stmt_with_span(StmtKind::Block(stmts), span_of_node(sync.syntax()))]
            }

            ast::Statement::AssertStatement(assert_stmt) => {
                // Best-effort: treat as an expression statement for the condition (+ optional message).
                let mut out = Vec::new();
                let exprs: Vec<_> = assert_stmt
                    .syntax()
                    .children()
                    .filter_map(ast::Expression::cast)
                    .collect();
                for expr in exprs {
                    let id = self.lower_expr(expr);
                    out.push(
                        self.builder
                            .stmt_with_span(StmtKind::Expr(id), span_of_node(assert_stmt.syntax())),
                    );
                }
                if out.is_empty() {
                    out.push(
                        self.builder
                            .stmt_with_span(StmtKind::Nop, span_of_node(assert_stmt.syntax())),
                    );
                }
                out
            }

            ast::Statement::ReturnStatement(ret) => {
                let value = ret.expression().map(|expr| self.lower_expr(expr));
                vec![self
                    .builder
                    .stmt_with_span(StmtKind::Return(value), span_of_node(ret.syntax()))]
            }

            ast::Statement::ThrowStatement(thr) => {
                let exception = thr
                    .expression()
                    .map(|expr| self.lower_expr(expr))
                    .unwrap_or_else(|| self.alloc_invalid_expr(span_of_node(thr.syntax())));
                vec![self
                    .builder
                    .stmt_with_span(StmtKind::Throw(exception), span_of_node(thr.syntax()))]
            }

            ast::Statement::BreakStatement(brk) => {
                let _ = brk.label_token();
                vec![self
                    .builder
                    .stmt_with_span(StmtKind::Break, span_of_node(brk.syntax()))]
            }

            ast::Statement::ContinueStatement(cont) => {
                let _ = cont.label_token();
                vec![self
                    .builder
                    .stmt_with_span(StmtKind::Continue, span_of_node(cont.syntax()))]
            }

            ast::Statement::ExplicitConstructorInvocation(inv) => {
                let expr = inv
                    .call()
                    .map(|call| self.lower_expr(ast::Expression::MethodCallExpression(call)))
                    .unwrap_or_else(|| self.alloc_invalid_expr(stmt_span));
                vec![self.builder.stmt_with_span(StmtKind::Expr(expr), stmt_span)]
            }

            ast::Statement::LocalTypeDeclarationStatement(local) => {
                // Best-effort: local class/interface/enum declarations do not directly impact the
                // method's control-flow (their bodies are checked elsewhere).
                vec![self
                    .builder
                    .stmt_with_span(StmtKind::Nop, span_of_node(local.syntax()))]
            }

            ast::Statement::LocalVariableDeclarationStatement(local) => {
                self.lower_local_declaration(&local)
            }

            ast::Statement::ExpressionStatement(expr_stmt) => {
                let span = span_of_node(expr_stmt.syntax());
                let Some(expr) = expr_stmt.expression() else {
                    let invalid = self.alloc_invalid_expr(span);
                    return vec![self.builder.stmt_with_span(StmtKind::Expr(invalid), span)];
                };

                self.lower_expression_statement(expr, span)
            }

            ast::Statement::EmptyStatement(empty) => vec![self
                .builder
                .stmt_with_span(StmtKind::Nop, span_of_node(empty.syntax()))],

            _ => vec![self.builder.stmt_with_span(StmtKind::Nop, stmt_span)],
        }
    }

    fn lower_expression_statement(&mut self, expr: ast::Expression, span: Span) -> Vec<StmtId> {
        match expr {
            ast::Expression::AssignmentExpression(assign) => {
                let lhs = assign.lhs();
                let rhs = assign.rhs();
                if let (Some(lhs), Some(rhs)) = (lhs, rhs) {
                    let rhs_id = self.lower_expr(rhs);
                    match lhs {
                        ast::Expression::NameExpression(name) => {
                            if let Some(target) = self.name_expr_local(&name) {
                                return vec![self.builder.stmt_with_span(
                                    StmtKind::Assign {
                                        target,
                                        value: rhs_id,
                                    },
                                    span,
                                )];
                            }

                            // Unknown assignment target: lower as a generic expression so we still
                            // traverse subexpressions for use-before-assignment diagnostics.
                            let lhs_id = self.lower_expr(ast::Expression::NameExpression(name));
                            let expr_id = self.builder.expr_with_span(
                                ExprKind::Invalid {
                                    children: vec![lhs_id, rhs_id],
                                },
                                span,
                            );
                            return vec![self
                                .builder
                                .stmt_with_span(StmtKind::Expr(expr_id), span)];
                        }
                        other_lhs => {
                            let lhs_id = self.lower_expr(other_lhs);
                            let expr_id = self.builder.expr_with_span(
                                ExprKind::Invalid {
                                    children: vec![lhs_id, rhs_id],
                                },
                                span,
                            );
                            return vec![self
                                .builder
                                .stmt_with_span(StmtKind::Expr(expr_id), span)];
                        }
                    }
                }

                let expr_id = self.alloc_invalid_expr(span);
                vec![self.builder.stmt_with_span(StmtKind::Expr(expr_id), span)]
            }
            other => {
                let expr_id = self.lower_expr(other);
                vec![self.builder.stmt_with_span(StmtKind::Expr(expr_id), span)]
            }
        }
    }

    fn lower_local_declaration(
        &mut self,
        local: &ast::LocalVariableDeclarationStatement,
    ) -> Vec<StmtId> {
        let Some(decls) = local.declarator_list() else {
            return vec![self
                .builder
                .stmt_with_span(StmtKind::Nop, span_of_node(local.syntax()))];
        };

        let mut out = Vec::new();
        for declarator in decls.declarators() {
            self.check_cancelled();
            let name_tok = declarator.name_token();
            let Some(name_tok) = name_tok else {
                continue;
            };

            let name_text = name_tok.text();
            let name = Name::new(name_text.to_string());
            let local_id = self.declare_local(name, LocalKind::Local, span_of_token(&name_tok));

            let init = declarator.initializer().map(|expr| self.lower_expr(expr));
            out.push(self.builder.stmt_with_span(
                StmtKind::Let {
                    local: local_id,
                    initializer: init,
                },
                span_of_node(declarator.syntax()),
            ));
        }

        if out.is_empty() {
            out.push(
                self.builder
                    .stmt_with_span(StmtKind::Nop, span_of_node(local.syntax())),
            );
        }
        out
    }

    fn lower_for_statement(&mut self, for_stmt: &ast::ForStatement) -> Vec<StmtId> {
        let header = for_stmt.header();
        let body_stmt = for_stmt.body();

        // The loop header introduces a scope (e.g. `for (int i = 0; ...) { ... }`).
        self.push_scope();

        let (init, condition, update) = header
            .as_ref()
            .map(|header| self.lower_for_header(header))
            .unwrap_or((None, None, None));

        let body = body_stmt
            .map(|stmt| self.lower_scoped_statement(stmt))
            .unwrap_or_else(|| {
                self.builder
                    .stmt_with_span(StmtKind::Nop, span_of_node(for_stmt.syntax()))
            });

        self.pop_scope();

        vec![self.builder.stmt_with_span(
            StmtKind::For {
                init,
                condition,
                update,
                body,
            },
            span_of_node(for_stmt.syntax()),
        )]
    }

    fn lower_for_header(
        &mut self,
        header: &ast::ForHeader,
    ) -> (Option<StmtId>, Option<ExprId>, Option<StmtId>) {
        self.check_cancelled();
        let header_node = header.syntax();

        // Enhanced-for has no `;` tokens directly under the header node.
        let has_semicolons =
            nova_syntax::ast::support::token(header_node, SyntaxKind::Semicolon).is_some();

        if !has_semicolons {
            // Enhanced for: `for (T x : expr)`.
            // Best-effort desugar: init declares + assigns the loop variable, condition is unknown.
            let init = header_node
                .children()
                .find_map(ast::VariableDeclaratorList::cast)
                .and_then(|decls| decls.declarators().next())
                .and_then(|decl| {
                    let name_tok = decl.name_token()?;
                    let name = Name::new(name_tok.text().to_string());
                    let local =
                        self.declare_local(name, LocalKind::Local, span_of_token(&name_tok));

                    // The enhanced-for variable is definitely assigned when the body runs. We model this
                    // by giving it an initializer.
                    let iterable_expr = header_node
                        .children()
                        .filter_map(ast::Expression::cast)
                        .next()
                        .map(|expr| self.lower_expr(expr))
                        .unwrap_or_else(|| self.alloc_invalid_expr(span_of_node(header_node)));

                    Some(self.builder.stmt_with_span(
                        StmtKind::Let {
                            local,
                            initializer: Some(iterable_expr),
                        },
                        span_of_node(decl.syntax()),
                    ))
                });

            let condition = Some(self.alloc_invalid_expr(span_of_node(header_node)));
            return (init, condition, None);
        }

        // Classic for: split expressions based on semicolon token offsets.
        let semis = header_node
            .children_with_tokens()
            .filter_map(|it| it.into_token())
            .filter(|tok| tok.kind() == SyntaxKind::Semicolon)
            .collect::<Vec<_>>();

        let first_semi = semis
            .first()
            .map(|t| u32::from(t.text_range().start()) as usize);
        let second_semi = semis
            .get(1)
            .map(|t| u32::from(t.text_range().start()) as usize);

        // Init.
        let init = if let Some(decls) = header_node
            .children()
            .find_map(ast::VariableDeclaratorList::cast)
        {
            // `for (int i = 0; ...`
            let mut init_stmts = Vec::new();
            for decl in decls.declarators() {
                let Some(name_tok) = decl.name_token() else {
                    continue;
                };
                let name = Name::new(name_tok.text().to_string());
                let local = self.declare_local(name, LocalKind::Local, span_of_token(&name_tok));
                let init = decl.initializer().map(|expr| self.lower_expr(expr));
                init_stmts.push(self.builder.stmt_with_span(
                    StmtKind::Let {
                        local,
                        initializer: init,
                    },
                    span_of_node(decl.syntax()),
                ));
            }
            if init_stmts.len() == 1 {
                init_stmts.pop()
            } else if init_stmts.is_empty() {
                None
            } else {
                Some(
                    self.builder
                        .stmt_with_span(StmtKind::Block(init_stmts), span_of_node(header_node)),
                )
            }
        } else {
            // Expression init list.
            let mut init_stmts = Vec::new();
            for expr in header_node.children().filter_map(ast::Expression::cast) {
                let expr_span = span_of_node(expr.syntax());
                let Some(first_semi) = first_semi else { break };
                if expr_span.start >= first_semi {
                    continue;
                }
                init_stmts.extend(self.lower_expression_statement(expr, expr_span));
            }
            if init_stmts.len() == 1 {
                init_stmts.pop()
            } else if init_stmts.is_empty() {
                None
            } else {
                Some(
                    self.builder
                        .stmt_with_span(StmtKind::Block(init_stmts), span_of_node(header_node)),
                )
            }
        };

        // Condition (best-effort pick the first expr between semicolons).
        let condition = header_node
            .children()
            .filter_map(ast::Expression::cast)
            .find(|expr| {
                let span = span_of_node(expr.syntax());
                let Some(first) = first_semi else {
                    return false;
                };
                let Some(second) = second_semi else {
                    return false;
                };
                span.start > first && span.start < second
            })
            .map(|expr| self.lower_expr(expr));

        // Update expr list.
        let mut update_stmts = Vec::new();
        for expr in header_node.children().filter_map(ast::Expression::cast) {
            let span = span_of_node(expr.syntax());
            let Some(second) = second_semi else { continue };
            if span.start <= second {
                continue;
            }
            update_stmts.extend(self.lower_expression_statement(expr, span));
        }
        let update = if update_stmts.len() == 1 {
            update_stmts.pop()
        } else if update_stmts.is_empty() {
            None
        } else {
            Some(
                self.builder
                    .stmt_with_span(StmtKind::Block(update_stmts), span_of_node(header_node)),
            )
        };

        (init, condition, update)
    }

    fn lower_switch_statement(&mut self, switch_stmt: &ast::SwitchStatement) -> Vec<StmtId> {
        self.check_cancelled();
        let span = span_of_node(switch_stmt.syntax());
        let expression = switch_stmt
            .expression()
            .map(|expr| self.lower_expr(expr))
            .unwrap_or_else(|| self.alloc_invalid_expr(span));

        let Some(block) = switch_stmt.block() else {
            return vec![self.builder.stmt_with_span(
                StmtKind::Switch {
                    expression,
                    arms: Vec::new(),
                },
                span,
            )];
        };

        // Switch statements introduce a scope. Colon-style `case:` groups share that scope, while
        // arrow rules (`case ->`) are lowered in their own nested scope to avoid leaking locals.
        self.push_scope();

        let mut arms = Vec::new();
        for group in block.syntax().children() {
            self.check_cancelled();
            let is_arrow = match group.kind() {
                SyntaxKind::SwitchGroup => false,
                SyntaxKind::SwitchRule => true,
                _ => continue,
            };

            let group_span = span_of_node(&group);

            let mut values = Vec::new();
            let mut has_default = false;

            for label in group.children().filter_map(ast::SwitchLabel::cast) {
                if node_has_token(label.syntax(), SyntaxKind::DefaultKw) {
                    has_default = true;
                }
                for element in label.elements() {
                    if node_has_token(element.syntax(), SyntaxKind::DefaultKw) {
                        has_default = true;
                        continue;
                    }
                    if let Some(expr) = element.expression() {
                        values.push(self.lower_expr(expr));
                    } else if let Some(pattern) = element.pattern() {
                        // Patterns are not modeled; still walk nested expressions so we can surface
                        // use-before-assignment diagnostics in best-effort fashion.
                        let _ = pattern;
                        for child in element
                            .syntax()
                            .children()
                            .filter_map(ast::Expression::cast)
                        {
                            let _ = self.lower_expr(child);
                        }
                    }
                }
            }

            let mut body_stmts = Vec::new();
            if is_arrow {
                self.push_scope();
            }

            for child in group.children() {
                if let Some(stmt) = ast::Statement::cast(child.clone()) {
                    body_stmts.extend(self.lower_statement(stmt));
                } else if let Some(expr) = ast::Expression::cast(child.clone()) {
                    let expr_id = self.lower_expr(expr);
                    body_stmts.push(
                        self.builder
                            .stmt_with_span(StmtKind::Expr(expr_id), span_of_node(&child)),
                    );
                }
            }

            if is_arrow {
                self.pop_scope();
            }

            let body = self
                .builder
                .stmt_with_span(StmtKind::Block(body_stmts), group_span);

            arms.push(SwitchArm {
                values,
                has_default,
                body,
                is_arrow,
            });
        }

        self.pop_scope();

        vec![self
            .builder
            .stmt_with_span(StmtKind::Switch { expression, arms }, span)]
    }

    fn lower_try_statement(&mut self, try_stmt: &ast::TryStatement) -> Vec<StmtId> {
        self.check_cancelled();
        let span = span_of_node(try_stmt.syntax());

        // Try-with-resources introduces resource variables scoped to the try statement.
        // Model this best-effort as:
        //   {
        //     <resource let stmts>
        //     try { ... } catch ... finally ...
        //   }
        //
        // This ensures resource vars are visible inside the try/catch/finally bodies but not after
        // the statement.
        if let Some(resource_spec) = try_stmt.resources() {
            self.push_scope();
            let mut block_stmts = Vec::new();

            for resource in resource_spec.resources() {
                self.check_cancelled();

                // `Resource` can be either a local-var declaration (modifiers + type + declarator)
                // or an expression (Java 9+).
                //
                // We only model the declaration form as a local because that's what makes the
                // resource name resolvable inside the try body.
                let ty = resource.syntax().children().find_map(ast::Type::cast);
                let decl = resource
                    .syntax()
                    .children()
                    .find_map(ast::VariableDeclarator::cast);

                if let (Some(_ty), Some(decl)) = (ty, decl) {
                    let Some(name_tok) = decl.name_token() else {
                        continue;
                    };

                    let name = Name::new(name_tok.text().to_string());
                    let local_id =
                        self.declare_local(name, LocalKind::Local, span_of_token(&name_tok));

                    // Resource variable declarations are definitely assigned (initializer required
                    // by the grammar); use an invalid expr for partial code.
                    let init_expr = decl
                        .initializer()
                        .map(|expr| self.lower_expr(expr))
                        .unwrap_or_else(|| {
                            self.alloc_invalid_expr(span_of_node(resource.syntax()))
                        });

                    block_stmts.push(self.builder.stmt_with_span(
                        StmtKind::Let {
                            local: local_id,
                            initializer: Some(init_expr),
                        },
                        span_of_node(resource.syntax()),
                    ));
                }
            }

            let body_block = try_stmt.body();
            let body = body_block
                .as_ref()
                .map(|block| self.lower_block(block))
                .unwrap_or_else(|| {
                    self.builder
                        .stmt_with_span(StmtKind::Nop, span_of_node(try_stmt.syntax()))
                });

            let mut catches = Vec::new();
            for catch in try_stmt.catches() {
                self.check_cancelled();
                self.push_scope();
                // Catch parameter is definitely assigned.
                if let Some(param_name) = catch_param_ident(&catch) {
                    let name = Name::new(param_name.text().to_string());
                    self.declare_local(name, LocalKind::Param, span_of_token(&param_name));
                }
                if let Some(block) = catch.body() {
                    catches.push(self.lower_block(&block));
                }
                self.pop_scope();
            }

            let finally = try_stmt
                .finally_clause()
                .and_then(|fin| fin.body())
                .map(|block| self.lower_block(&block));

            block_stmts.push(self.builder.stmt_with_span(
                StmtKind::Try {
                    body,
                    catches,
                    finally,
                },
                span,
            ));

            self.pop_scope();

            return vec![self
                .builder
                .stmt_with_span(StmtKind::Block(block_stmts), span)];
        }

        let body_block = try_stmt.body();
        let body = body_block
            .as_ref()
            .map(|block| self.lower_block(block))
            .unwrap_or_else(|| {
                self.builder
                    .stmt_with_span(StmtKind::Nop, span_of_node(try_stmt.syntax()))
            });

        let mut catches = Vec::new();
        for catch in try_stmt.catches() {
            self.check_cancelled();
            self.push_scope();
            // Catch parameter is definitely assigned.
            if let Some(param_name) = catch_param_ident(&catch) {
                let name = Name::new(param_name.text().to_string());
                self.declare_local(name, LocalKind::Param, span_of_token(&param_name));
            }
            if let Some(block) = catch.body() {
                catches.push(self.lower_block(&block));
            }
            self.pop_scope();
        }

        let finally = try_stmt
            .finally_clause()
            .and_then(|fin| fin.body())
            .map(|block| self.lower_block(&block));

        vec![self.builder.stmt_with_span(
            StmtKind::Try {
                body,
                catches,
                finally,
            },
            span,
        )]
    }

    fn lower_expr(&mut self, expr: ast::Expression) -> ExprId {
        self.check_cancelled();
        match expr {
            ast::Expression::LiteralExpression(lit) => self.lower_literal(&lit),

            ast::Expression::NameExpression(name) => {
                if let Some(local) = self.name_expr_local(&name) {
                    return self
                        .builder
                        .expr_with_span(ExprKind::Local(local), span_of_node(name.syntax()));
                }
                self.alloc_invalid_expr(span_of_node(name.syntax()))
            }

            ast::Expression::ThisExpression(this) => self.builder.expr_with_span(
                ExprKind::New {
                    class_name: "this".to_string(),
                    args: Vec::new(),
                },
                span_of_node(this.syntax()),
            ),

            ast::Expression::SuperExpression(sup) => self.builder.expr_with_span(
                ExprKind::New {
                    class_name: "super".to_string(),
                    args: Vec::new(),
                },
                span_of_node(sup.syntax()),
            ),

            ast::Expression::ParenthesizedExpression(par) => par
                .expression()
                .map(|inner| self.lower_expr(inner))
                .unwrap_or_else(|| self.alloc_invalid_expr(span_of_node(par.syntax()))),

            ast::Expression::NewExpression(new_expr) => {
                let mut args = Vec::new();
                if let Some(arguments) = new_expr.arguments() {
                    for arg in arguments.arguments() {
                        args.push(self.lower_expr(arg));
                    }
                }

                self.builder.expr_with_span(
                    ExprKind::New {
                        class_name: new_expr
                            .ty()
                            .map(|ty| ty.syntax().text().to_string())
                            .unwrap_or_else(|| "new".to_string()),
                        args,
                    },
                    span_of_node(new_expr.syntax()),
                )
            }

            ast::Expression::MethodCallExpression(call) => {
                let (receiver, name) = match call.callee() {
                    Some(ast::Expression::FieldAccessExpression(access)) => {
                        let receiver = access.expression().map(|expr| self.lower_expr(expr));
                        let name = access
                            .name_token()
                            .map(|tok| Name::new(tok.text().to_string()))
                            .unwrap_or_else(|| Name::new("<call>"));
                        (receiver, name)
                    }
                    Some(ast::Expression::NameExpression(name_expr)) => {
                        let mut name = String::new();
                        for tok in name_expr
                            .syntax()
                            .children_with_tokens()
                            .filter_map(|it| it.into_token())
                            .filter(|tok| !tok.kind().is_trivia())
                        {
                            name.push_str(tok.text());
                        }
                        let span = span_of_node(name_expr.syntax());

                        // If this is a qualified name like `s.length`, try to recover a receiver
                        // expression when the qualifier starts with a known local. This avoids
                        // missing null-deref diagnostics for common call forms like `s.length()`,
                        // while still treating `TypeName.staticCall()` as receiver-less.
                        if let Some((qualifier, method_name)) = name.rsplit_once('.') {
                            let mut segments = qualifier.split('.');
                            if let Some(first) = segments.next() {
                                if let Some(local) = self.lookup_local(first) {
                                    let mut receiver_expr =
                                        self.builder.expr_with_span(ExprKind::Local(local), span);
                                    for seg in segments {
                                        receiver_expr = self.builder.expr_with_span(
                                            ExprKind::FieldAccess {
                                                receiver: receiver_expr,
                                                name: Name::new(seg),
                                            },
                                            span,
                                        );
                                    }

                                    (Some(receiver_expr), Name::new(method_name))
                                } else {
                                    (None, Name::new(method_name))
                                }
                            } else {
                                (None, Name::new(method_name))
                            }
                        } else {
                            (None, Name::new(name))
                        }
                    }
                    Some(other) => (Some(self.lower_expr(other)), Name::new("<call>")),
                    None => (None, Name::new("<call>")),
                };

                let mut args = Vec::new();
                if let Some(arguments) = call.arguments() {
                    for arg in arguments.arguments() {
                        args.push(self.lower_expr(arg));
                    }
                }

                self.builder.expr_with_span(
                    ExprKind::Call {
                        receiver,
                        name,
                        args,
                    },
                    span_of_node(call.syntax()),
                )
            }

            ast::Expression::FieldAccessExpression(access) => {
                let receiver = access
                    .expression()
                    .map(|expr| self.lower_expr(expr))
                    .unwrap_or_else(|| self.alloc_invalid_expr(span_of_node(access.syntax())));
                let name = access
                    .name_token()
                    .map(|tok| Name::new(tok.text().to_string()))
                    .unwrap_or_else(|| Name::new("<field>"));
                self.builder.expr_with_span(
                    ExprKind::FieldAccess { receiver, name },
                    span_of_node(access.syntax()),
                )
            }

            ast::Expression::UnaryExpression(unary) => {
                let operand = unary
                    .operand()
                    .map(|expr| self.lower_expr(expr))
                    .unwrap_or_else(|| self.alloc_invalid_expr(span_of_node(unary.syntax())));
                let op = if nova_syntax::ast::support::token(unary.syntax(), SyntaxKind::Bang)
                    .is_some()
                {
                    Some(UnaryOp::Not)
                } else {
                    None
                };
                match op {
                    Some(op) => self.builder.expr_with_span(
                        ExprKind::Unary { op, expr: operand },
                        span_of_node(unary.syntax()),
                    ),
                    None => self.builder.expr_with_span(
                        ExprKind::Invalid {
                            children: vec![operand],
                        },
                        span_of_node(unary.syntax()),
                    ),
                }
            }

            ast::Expression::BinaryExpression(binary) => {
                let lhs = binary
                    .lhs()
                    .map(|expr| self.lower_expr(expr))
                    .unwrap_or_else(|| self.alloc_invalid_expr(span_of_node(binary.syntax())));
                let rhs = binary
                    .rhs()
                    .map(|expr| self.lower_expr(expr))
                    .unwrap_or_else(|| self.alloc_invalid_expr(span_of_node(binary.syntax())));

                let op = binary_op_token(binary.syntax());
                match op {
                    Some(op) => self.builder.expr_with_span(
                        ExprKind::Binary { op, lhs, rhs },
                        span_of_node(binary.syntax()),
                    ),
                    None => self.builder.expr_with_span(
                        ExprKind::Invalid {
                            children: vec![lhs, rhs],
                        },
                        span_of_node(binary.syntax()),
                    ),
                }
            }

            // Best-effort: preserve child expressions so flow analysis can still walk locals.
            ast::Expression::ArrayAccessExpression(access) => {
                let mut children = Vec::new();
                if let Some(array) = access.array() {
                    children.push(self.lower_expr(array));
                }
                if let Some(index) = access.index() {
                    children.push(self.lower_expr(index));
                }
                self.builder.expr_with_span(
                    ExprKind::Invalid { children },
                    span_of_node(access.syntax()),
                )
            }
            ast::Expression::InstanceofExpression(inst) => {
                let mut children = Vec::new();
                if let Some(lhs) = inst.lhs() {
                    children.push(self.lower_expr(lhs));
                }
                self.builder
                    .expr_with_span(ExprKind::Invalid { children }, span_of_node(inst.syntax()))
            }
            ast::Expression::AssignmentExpression(assign) => {
                let mut children = Vec::new();
                if let Some(lhs) = assign.lhs() {
                    children.push(self.lower_expr(lhs));
                }
                if let Some(rhs) = assign.rhs() {
                    children.push(self.lower_expr(rhs));
                }
                self.builder.expr_with_span(
                    ExprKind::Invalid { children },
                    span_of_node(assign.syntax()),
                )
            }
            ast::Expression::ConditionalExpression(cond) => {
                let mut children = Vec::new();
                if let Some(c) = cond.condition() {
                    children.push(self.lower_expr(c));
                }
                if let Some(t) = cond.then_branch() {
                    children.push(self.lower_expr(t));
                }
                if let Some(e) = cond.else_branch() {
                    children.push(self.lower_expr(e));
                }
                self.builder
                    .expr_with_span(ExprKind::Invalid { children }, span_of_node(cond.syntax()))
            }
            ast::Expression::SwitchExpression(switch_expr) => {
                // Best-effort: lower `switch (expr) { ... }` expressions by collecting nested
                // expressions so downstream analyses (e.g. Extract Method) can still observe local
                // reads inside arms.
                //
                // We intentionally skip nested lambda bodies to avoid treating lazily-executed code
                // as eagerly evaluated in flow analysis (consistent with `LambdaExpression`).
                let span = span_of_node(switch_expr.syntax());
                let mut children = Vec::new();

                let selector = switch_expr
                    .expression()
                    .map(|expr| self.lower_expr(expr))
                    .unwrap_or_else(|| self.alloc_invalid_expr(span));
                children.push(selector);

                let Some(block) = switch_expr.block() else {
                    return self
                        .builder
                        .expr_with_span(ExprKind::Invalid { children }, span);
                };

                fn inside_lambda_body(node: &SyntaxNode) -> bool {
                    let mut cur = node.parent();
                    while let Some(parent) = cur {
                        if parent.kind() == SyntaxKind::LambdaExpression {
                            return true;
                        }
                        cur = parent.parent();
                    }
                    false
                }

                for group in block.syntax().children() {
                    self.check_cancelled();

                    match group.kind() {
                        SyntaxKind::SwitchGroup | SyntaxKind::SwitchRule => {}
                        _ => continue,
                    }

                    // Case label expressions (e.g. `case 0, x -> ...`) and guards (`when ...`).
                    for label in group.children().filter_map(ast::SwitchLabel::cast) {
                        for element in label.elements() {
                            if node_has_token(element.syntax(), SyntaxKind::DefaultKw) {
                                continue;
                            }
                            if let Some(expr) = element.expression() {
                                children.push(self.lower_expr(expr));
                            } else if let Some(pattern) = element.pattern() {
                                // Patterns are not modeled; still walk any nested expressions so we
                                // can observe local reads (best-effort).
                                let _ = pattern;
                                for child in element
                                    .syntax()
                                    .children()
                                    .filter_map(ast::Expression::cast)
                                {
                                    children.push(self.lower_expr(child));
                                }
                            }
                            if let Some(guard) = element.guard().and_then(|g| g.expression()) {
                                children.push(self.lower_expr(guard));
                            }
                        }
                    }

                    // Walk expressions inside the arm body.
                    if group.kind() == SyntaxKind::SwitchGroup {
                        for stmt in group.children().filter_map(ast::Statement::cast) {
                            for expr in stmt
                                .syntax()
                                .descendants()
                                .filter_map(ast::Expression::cast)
                                .filter(|expr| !inside_lambda_body(expr.syntax()))
                            {
                                children.push(self.lower_expr(expr));
                            }
                        }
                    } else if let Some(rule) = ast::SwitchRule::cast(group.clone()) {
                        if let Some(body) = rule.body() {
                            // If the body is an expression (`case ... -> <expr>`), include it
                            // directly (in addition to its descendants).
                            if let ast::SwitchRuleBody::Expression(expr) = body.clone() {
                                children.push(self.lower_expr(expr));
                            }

                            for expr in body
                                .syntax()
                                .descendants()
                                .filter_map(ast::Expression::cast)
                                .filter(|expr| !inside_lambda_body(expr.syntax()))
                            {
                                children.push(self.lower_expr(expr));
                            }
                        }
                    }
                }

                self.builder
                    .expr_with_span(ExprKind::Invalid { children }, span)
            }
            ast::Expression::CastExpression(cast) => {
                let mut children = Vec::new();
                if let Some(expr) = cast.expression() {
                    children.push(self.lower_expr(expr));
                }
                self.builder
                    .expr_with_span(ExprKind::Invalid { children }, span_of_node(cast.syntax()))
            }
            ast::Expression::LambdaExpression(lambda) => {
                // Lambda bodies are executed lazily; skip lowering their internals (best-effort).
                let _ = lambda.body();
                self.alloc_invalid_expr(span_of_node(lambda.syntax()))
            }

            #[allow(unreachable_patterns)]
            other => {
                // Best-effort fallback: lower any child expressions so flow analysis can still
                // inspect local reads nested inside unsupported constructs.
                let children = other
                    .syntax()
                    .children()
                    .filter_map(ast::Expression::cast)
                    .map(|child| self.lower_expr(child))
                    .collect::<Vec<_>>();
                self.builder
                    .expr_with_span(ExprKind::Invalid { children }, span_of_node(other.syntax()))
            }
        }
    }

    fn lower_literal(&mut self, lit: &ast::LiteralExpression) -> ExprId {
        let span = span_of_node(lit.syntax());
        // Find the first non-trivia token inside the literal.
        let token = lit
            .syntax()
            .children_with_tokens()
            .filter_map(|it| it.into_token())
            .find(|tok| !tok.kind().is_trivia());
        let Some(tok) = token else {
            return self.alloc_invalid_expr(span);
        };

        let kind = match tok.kind() {
            SyntaxKind::NullKw => ExprKind::Null,
            SyntaxKind::TrueKw => ExprKind::Bool(true),
            SyntaxKind::FalseKw => ExprKind::Bool(false),
            SyntaxKind::IntLiteral | SyntaxKind::Number => {
                let raw = tok.text().replace('_', "");
                let value = raw.parse::<i32>().unwrap_or(0);
                ExprKind::Int(value)
            }
            SyntaxKind::StringLiteral => ExprKind::String(tok.text().to_string()),
            _ => ExprKind::Invalid {
                children: Vec::new(),
            },
        };

        self.builder.expr_with_span(kind, span)
    }

    fn alloc_invalid_expr(&mut self, span: Span) -> ExprId {
        self.builder.expr_with_span(
            ExprKind::Invalid {
                children: Vec::new(),
            },
            span,
        )
    }

    fn name_expr_local(&self, name: &ast::NameExpression) -> Option<LocalId> {
        // Name expressions wrap a `Name` node; fall back to the first identifier token.
        let full = name
            .syntax()
            .children()
            .find_map(ast::Name::cast)
            .map(|name| name.text())
            .or_else(|| {
                nova_syntax::ast::support::ident_token(name.syntax())
                    .map(|tok| tok.text().to_string())
            })?;
        let simple = full.rsplit('.').next().unwrap_or(&full);
        self.lookup_local(simple)
    }
}

fn span_of_node(node: &SyntaxNode) -> Span {
    let range = node.text_range();
    Span::new(
        u32::from(range.start()) as usize,
        u32::from(range.end()) as usize,
    )
}

fn span_of_token(token: &SyntaxToken) -> Span {
    let range = token.text_range();
    Span::new(
        u32::from(range.start()) as usize,
        u32::from(range.end()) as usize,
    )
}

fn binary_op_token(node: &SyntaxNode) -> Option<BinaryOp> {
    // `BinaryExpression` does not currently expose the operator token directly.
    // Scan direct children for the first binary operator we care about.
    node.children_with_tokens()
        .filter_map(|it| it.into_token())
        .find_map(|tok| match tok.kind() {
            SyntaxKind::EqEq => Some(BinaryOp::EqEq),
            SyntaxKind::BangEq => Some(BinaryOp::NotEq),
            SyntaxKind::AmpAmp => Some(BinaryOp::AndAnd),
            SyntaxKind::PipePipe => Some(BinaryOp::OrOr),
            _ => None,
        })
}

fn node_has_token(node: &SyntaxNode, kind: SyntaxKind) -> bool {
    nova_syntax::ast::support::token(node, kind).is_some()
}

fn catch_param_ident(catch: &ast::CatchClause) -> Option<SyntaxToken> {
    let before = catch
        .body()
        .map(|body| u32::from(body.syntax().text_range().start()) as usize);

    let mut out = None;
    for tok in nova_syntax::ast::support::ident_tokens(catch.syntax()) {
        if let Some(before) = before {
            let end = u32::from(tok.text_range().end()) as usize;
            if end > before {
                break;
            }
        }
        out = Some(tok);
    }
    out
}
