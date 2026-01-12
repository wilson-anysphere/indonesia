use nova_db::salsa::{NovaFlow, NovaHir};
use nova_db::{FileId, SalsaRootDatabase};
use nova_hir::body::{Body as FlowBody, ExprKind as FlowExprKind, StmtKind as FlowStmtKind};
use nova_hir::hir::{Body, Expr, ExprId, Stmt};

fn expr_path(body: &Body, expr: ExprId) -> Option<String> {
    match &body.exprs[expr] {
        Expr::Name { name, .. } => Some(name.clone()),
        Expr::FieldAccess { receiver, name, .. } => {
            let mut path = expr_path(body, *receiver)?;
            path.push('.');
            path.push_str(name);
            Some(path)
        }
        _ => None,
    }
}

#[test]
fn hir_item_tree_captures_package_imports_and_types() {
    let source = r#"
package com.example;

import java.util.List;
import java.util.*;
import static java.lang.Math.*;
import static java.lang.Math.PI;

@interface Marker {
    int value() default 1;
}

class Foo {
    int field;

    void bar(final int y) {
        final int z = y + 1;
        System.out.println(z);
        return;
    }
    }
"#;

    let mut db = SalsaRootDatabase::default();
    let file = FileId::from_raw(0);
    db.set_file_text(file, source);

    let snap = db.snapshot();
    let tree = snap.hir_item_tree(file);

    let pkg = tree.package.as_ref().expect("package");
    assert_eq!(pkg.name, "com.example");

    assert_eq!(tree.imports.len(), 4);
    assert!(tree
        .imports
        .iter()
        .any(|import| !import.is_static && !import.is_star && import.path == "java.util.List"));
    assert!(tree
        .imports
        .iter()
        .any(|import| !import.is_static && import.is_star && import.path == "java.util"));
    assert!(tree
        .imports
        .iter()
        .any(|import| import.is_static && import.is_star && import.path == "java.lang.Math"));
    assert!(tree
        .imports
        .iter()
        .any(|import| import.is_static && !import.is_star && import.path == "java.lang.Math.PI"));

    assert!(tree
        .items
        .iter()
        .any(|item| matches!(item, nova_hir::item_tree::Item::Annotation(_))));
    assert!(tree
        .items
        .iter()
        .any(|item| matches!(item, nova_hir::item_tree::Item::Class(_))));
}

#[test]
fn hir_method_body_lowers_locals_and_calls() {
    let source = r#"
class Foo {
    void bar(final int y) {
        final int z = y + 1;
        System.out.println(z);
        return;
    }
}
"#;

    let mut db = SalsaRootDatabase::default();
    let file = FileId::from_raw(0);
    db.set_file_text(file, source);

    let snap = db.snapshot();
    let tree = snap.hir_item_tree(file);
    let (&bar_ast_id, _) = tree
        .methods
        .iter()
        .find(|(_, method)| method.name == "bar")
        .expect("bar method");
    let bar_id = nova_hir::ids::MethodId::new(file, bar_ast_id);
    let body = snap.hir_body(bar_id);

    let local_names: Vec<_> = body
        .locals
        .iter()
        .map(|(_, local)| local.name.as_str())
        .collect();
    assert_eq!(local_names, vec!["z"]);

    let mut call_paths = Vec::new();
    for (id, expr) in body.exprs.iter() {
        if let Expr::Call { callee, .. } = expr {
            let callee_path = expr_path(&body, *callee).unwrap_or_else(|| format!("ExprId({id})"));
            call_paths.push(callee_path);
        }
    }
    assert!(call_paths.iter().any(|path| path == "System.out.println"));
}

#[test]
fn hir_method_body_lowers_synchronized_statement() {
    let source = r#"
class C {
    void m() {
        Object x = new Object();
        synchronized (x) { int y = 0; }
    }
}
"#;

    let mut db = SalsaRootDatabase::default();
    let file = FileId::from_raw(0);
    db.set_file_text(file, source);

    let snap = db.snapshot();
    let tree = snap.hir_item_tree(file);
    let (&method_ast_id, _) = tree
        .methods
        .iter()
        .find(|(_, method)| method.name == "m")
        .expect("m method");
    let method_id = nova_hir::ids::MethodId::new(file, method_ast_id);
    let body = snap.hir_body(method_id);

    let mut saw_synchronized = false;
    for (_, stmt) in body.stmts.iter() {
        let Stmt::Synchronized {
            expr: lock_expr,
            body: sync_body,
            ..
        } = stmt
        else {
            continue;
        };

        saw_synchronized = true;

        let Expr::Name { name, .. } = &body.exprs[*lock_expr] else {
            panic!(
                "expected synchronized lock expression to lower to a name expression, got {:?}",
                body.exprs[*lock_expr]
            );
        };
        assert_eq!(name, "x");

        let Stmt::Block { statements, .. } = &body.stmts[*sync_body] else {
            panic!(
                "expected synchronized body to lower to a block statement, got {:?}",
                body.stmts[*sync_body]
            );
        };
        assert!(
            !statements.is_empty(),
            "expected synchronized body block to contain statements"
        );

        assert!(
            body.locals.iter().any(|(_, local)| local.name == "y"),
            "expected synchronized body local `y` to be lowered, locals: {:?}",
            body.locals
                .iter()
                .map(|(_, l)| l.name.as_str())
                .collect::<Vec<_>>()
        );
        break;
    }

    assert!(
        saw_synchronized,
        "expected HIR body to contain a Synchronized statement"
    );
}

#[test]
fn flow_constructor_body_lowers_explicit_constructor_invocation() {
    let source = r#"
class C {
    C() { this(1); }
    C(int x) {}
}
"#;

    let mut db = SalsaRootDatabase::default();
    let file = FileId::from_raw(0);
    db.set_file_text(file, source);

    let snap = db.snapshot();
    let tree = snap.hir_item_tree(file);
    let (&ctor_ast_id, _) = tree
        .constructors
        .iter()
        .find(|(_, ctor)| ctor.params.is_empty())
        .expect("expected a no-arg constructor");
    let ctor_id = nova_hir::ids::ConstructorId::new(file, ctor_ast_id);

    let body: std::sync::Arc<FlowBody> = snap.flow_body_constructor(ctor_id);
    let FlowStmtKind::Block(stmts) = &body.stmt(body.root()).kind else {
        panic!("expected flow body root to be a block");
    };

    let mut saw_invocation = false;
    for stmt in stmts {
        if let FlowStmtKind::Expr(expr) = &body.stmt(*stmt).kind {
            if let FlowExprKind::Call { args, .. } = &body.expr(*expr).kind {
                if args.len() == 1 && matches!(body.expr(args[0]).kind, FlowExprKind::Int(1)) {
                    saw_invocation = true;
                    break;
                }
            }
        }
    }

    assert!(
        saw_invocation,
        "expected flow body to contain a call expression statement for `this(1);`"
    );
}

fn flow_diagnostics_for_method(source: &str, method_name: &str) -> Vec<nova_types::Diagnostic> {
    let mut db = SalsaRootDatabase::default();
    let file = FileId::from_raw(0);
    db.set_file_text(file, source);

    let snap = db.snapshot();
    let tree = snap.hir_item_tree(file);
    let (&method_ast_id, _) = tree
        .methods
        .iter()
        .find(|(_, method)| method.name == method_name)
        .unwrap_or_else(|| panic!("expected method `{method_name}` to exist"));
    let method_id = nova_hir::ids::MethodId::new(file, method_ast_id);

    snap.flow_diagnostics(method_id).as_ref().clone()
}

fn assert_has_diagnostic_code(diags: &[nova_types::Diagnostic], code: &str) {
    assert!(
        diags.iter().any(|diag| diag.code.as_ref() == code),
        "expected diagnostic with code `{code}`, got: {diags:#?}",
    );
}

fn flow_diagnostics_for_constructor(
    source: &str,
    wants_params: bool,
) -> Vec<nova_types::Diagnostic> {
    let mut db = SalsaRootDatabase::default();
    let file = FileId::from_raw(0);
    db.set_file_text(file, source);

    let snap = db.snapshot();
    let tree = snap.hir_item_tree(file);
    let (&ctor_ast_id, _) = tree
        .constructors
        .iter()
        .find(|(_, ctor)| ctor.params.is_empty() != wants_params)
        .unwrap_or_else(|| panic!("expected constructor with wants_params={wants_params}"));
    let ctor_id = nova_hir::ids::ConstructorId::new(file, ctor_ast_id);

    snap.flow_diagnostics_constructor(ctor_id).as_ref().clone()
}

#[test]
fn flow_diagnostics_reports_unreachable_code() {
    let source = r#"
class Foo {
    void m() {
        return;
        int x = 1;
    }
}
"#;

    let diags = flow_diagnostics_for_method(source, "m");
    assert_has_diagnostic_code(&diags, "FLOW_UNREACHABLE");
}

#[test]
fn flow_diagnostics_reports_use_before_assignment() {
    let source = r#"
class Foo {
    void m() {
        int x;
        System.out.println(x);
    }
}
"#;

    let diags = flow_diagnostics_for_method(source, "m");
    assert_has_diagnostic_code(&diags, "FLOW_UNASSIGNED");
}

#[test]
fn flow_diagnostics_reports_possible_null_dereference() {
    let source = r#"
class Foo {
    void m(String s) {
        s = null;
        s.length();
    }
}
"#;

    let diags = flow_diagnostics_for_method(source, "m");
    assert_has_diagnostic_code(&diags, "FLOW_NULL_DEREF");
}

#[test]
fn flow_diagnostics_constructor_reports_null_deref_in_explicit_constructor_invocation() {
    let source = r#"
class C {
    C(String s) {}
    C() { this(((String)null).toString()); }
}
"#;

    let diags = flow_diagnostics_for_constructor(source, false);
    assert_has_diagnostic_code(&diags, "FLOW_NULL_DEREF");
}
