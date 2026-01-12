use std::collections::HashSet;

use nova_core::Name;
use nova_hir::body::{Body, ExprId, ExprKind, LocalKind, StmtId, StmtKind};
use nova_hir::body_lowering::lower_flow_body;
use nova_syntax::ast::{self, AstNode};
use nova_syntax::parse_java_block_fragment;
use nova_types::Span;

fn parse_block(text: &str) -> ast::Block {
    let parsed = parse_java_block_fragment(text, 0);
    assert!(
        parsed.parse.errors.is_empty(),
        "unexpected parse errors: {:?}",
        parsed.parse.errors
    );

    parsed
        .parse
        .syntax()
        .descendants()
        .find_map(ast::Block::cast)
        .expect("expected a Block node in fragment")
}

fn collect_local_reads_in_expr(body: &Body, expr: ExprId, out: &mut HashSet<usize>) {
    match &body.expr(expr).kind {
        ExprKind::Local(local) => {
            out.insert(local.index());
        }
        ExprKind::Null | ExprKind::Bool(_) | ExprKind::Int(_) | ExprKind::String(_) => {}
        ExprKind::New { args, .. } => {
            for arg in args {
                collect_local_reads_in_expr(body, *arg, out);
            }
        }
        ExprKind::Unary { expr, .. } => collect_local_reads_in_expr(body, *expr, out),
        ExprKind::Binary { lhs, rhs, .. } => {
            collect_local_reads_in_expr(body, *lhs, out);
            collect_local_reads_in_expr(body, *rhs, out);
        }
        ExprKind::FieldAccess { receiver, .. } => collect_local_reads_in_expr(body, *receiver, out),
        ExprKind::Call { receiver, args, .. } => {
            if let Some(receiver) = receiver {
                collect_local_reads_in_expr(body, *receiver, out);
            }
            for arg in args {
                collect_local_reads_in_expr(body, *arg, out);
            }
        }
        ExprKind::Invalid { children } => {
            for child in children {
                collect_local_reads_in_expr(body, *child, out);
            }
        }
    }
}

fn collect_local_reads_in_stmt(body: &Body, stmt: StmtId, out: &mut HashSet<usize>) {
    match &body.stmt(stmt).kind {
        StmtKind::Block(stmts) => {
            for stmt in stmts {
                collect_local_reads_in_stmt(body, *stmt, out);
            }
        }
        StmtKind::Let { initializer, .. } => {
            if let Some(expr) = initializer {
                collect_local_reads_in_expr(body, *expr, out);
            }
        }
        StmtKind::Assign { value, .. } => collect_local_reads_in_expr(body, *value, out),
        StmtKind::Expr(expr) => collect_local_reads_in_expr(body, *expr, out),
        StmtKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_local_reads_in_expr(body, *condition, out);
            collect_local_reads_in_stmt(body, *then_branch, out);
            if let Some(else_branch) = else_branch {
                collect_local_reads_in_stmt(body, *else_branch, out);
            }
        }
        StmtKind::While { condition, body: b } => {
            collect_local_reads_in_expr(body, *condition, out);
            collect_local_reads_in_stmt(body, *b, out);
        }
        StmtKind::DoWhile { body: b, condition } => {
            collect_local_reads_in_stmt(body, *b, out);
            collect_local_reads_in_expr(body, *condition, out);
        }
        StmtKind::For {
            init,
            condition,
            update,
            body: b,
        } => {
            if let Some(init) = init {
                collect_local_reads_in_stmt(body, *init, out);
            }
            if let Some(condition) = condition {
                collect_local_reads_in_expr(body, *condition, out);
            }
            if let Some(update) = update {
                collect_local_reads_in_stmt(body, *update, out);
            }
            collect_local_reads_in_stmt(body, *b, out);
        }
        StmtKind::Switch { expression, arms } => {
            collect_local_reads_in_expr(body, *expression, out);
            for arm in arms {
                for value in &arm.values {
                    collect_local_reads_in_expr(body, *value, out);
                }
                collect_local_reads_in_stmt(body, arm.body, out);
            }
        }
        StmtKind::Try {
            body: b,
            catches,
            finally,
        } => {
            collect_local_reads_in_stmt(body, *b, out);
            for catch in catches {
                collect_local_reads_in_stmt(body, *catch, out);
            }
            if let Some(finally) = finally {
                collect_local_reads_in_stmt(body, *finally, out);
            }
        }
        StmtKind::Return(expr) => {
            if let Some(expr) = expr {
                collect_local_reads_in_expr(body, *expr, out);
            }
        }
        StmtKind::Throw(expr) => collect_local_reads_in_expr(body, *expr, out),
        StmtKind::Break | StmtKind::Continue | StmtKind::Nop => {}
    }
}

#[test]
fn switch_expression_arms_are_visited_in_flow_lowering() {
    let block = parse_block(
        "{ int y = switch (0) { case 0 -> x + 1; default -> x + 2; }; }",
    );

    let body = lower_flow_body(&block, [(Name::new("x"), Span::new(0, 0))]);

    let x_local_index = body
        .locals()
        .iter()
        .position(|local| local.name.as_str() == "x" && local.kind == LocalKind::Param)
        .expect("expected param local `x` to be declared");

    let mut reads = HashSet::new();
    collect_local_reads_in_stmt(&body, body.root(), &mut reads);

    assert!(
        reads.contains(&x_local_index),
        "expected flow lowering to record reads of `x` inside switch expression arms"
    );
}

