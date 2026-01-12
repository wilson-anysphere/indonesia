use std::sync::Arc;

use nova_db::salsa::FileExprId;
use nova_db::{
    ArcEq, FileId, NovaHir, NovaInputs, NovaTypeck, ProjectId, SalsaRootDatabase, SourceRootId,
};
use nova_jdk::JdkIndex;
use nova_resolve::ids::DefWithBodyId;
use nova_types::format_type;

fn setup_db(text: &str) -> (SalsaRootDatabase, FileId) {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let file = FileId::from_raw(1);
    db.set_file_project(file, project);
    db.set_file_rel_path(file, Arc::new("src/Test.java".to_string()));
    db.set_source_root(file, SourceRootId::from_raw(0));
    db.set_file_exists(file, true);
    db.set_file_is_dirty(file, false);
    db.set_file_text(file, text.to_string());

    db.set_project_files(project, Arc::new(vec![file]));
    db.set_all_file_ids(Arc::new(vec![file]));
    (db, file)
}

#[test]
fn demand_type_of_expr_does_not_execute_typeck_body() {
    let src = r#"
class C {
    String m() {
        var s = "x";
        return s.substring(1);
    }
}
"#;

    let (db, file) = setup_db(src);

    let tree = db.hir_item_tree(file);
    let method_ast = tree
        .methods
        .iter()
        .find_map(|(ast_id, m)| (m.name == "m" && m.body.is_some()).then_some(*ast_id))
        .expect("expected method `m` with a body");
    let method_id = nova_hir::ids::MethodId::new(file, method_ast);
    let owner = DefWithBodyId::Method(method_id);

    let body = db.hir_body(method_id);
    let root = &body.stmts[body.root];
    let return_expr = match root {
        nova_hir::hir::Stmt::Block { statements, .. } => statements
            .iter()
            .find_map(|stmt| match &body.stmts[*stmt] {
                nova_hir::hir::Stmt::Return {
                    expr: Some(expr), ..
                } => Some(*expr),
                _ => None,
            })
            .expect("expected a return expression"),
        other => panic!("expected a block root statement, got {other:?}"),
    };

    db.clear_query_stats();
    let _ = db.type_of_expr_demand(
        file,
        FileExprId {
            owner,
            expr: return_expr,
        },
    );

    let demand_res = db.type_of_expr_demand_result(
        file,
        FileExprId {
            owner,
            expr: return_expr,
        },
    );
    assert_eq!(
        format_type(&*demand_res.env, &demand_res.ty),
        "String",
        "expected demand-driven inference to resolve the substring call return type"
    );

    let stats = db.query_stats();
    assert!(
        stats.by_query.get("typeck_body").is_none(),
        "type_of_expr_demand should not execute typeck_body; stats: {:?}",
        stats.by_query.get("typeck_body")
    );
}

#[test]
fn type_of_expr_is_demand_driven_and_does_not_execute_typeck_body() {
    let src = r#"
class C {
    String m() {
        var s = "x";
        return s.substring(1);
    }
}
"#;

    let (db, file) = setup_db(src);

    let tree = db.hir_item_tree(file);
    let method_ast = tree
        .methods
        .iter()
        .find_map(|(ast_id, m)| (m.name == "m" && m.body.is_some()).then_some(*ast_id))
        .expect("expected method `m` with a body");
    let method_id = nova_hir::ids::MethodId::new(file, method_ast);
    let owner = DefWithBodyId::Method(method_id);

    let body = db.hir_body(method_id);
    let root = &body.stmts[body.root];
    let return_expr = match root {
        nova_hir::hir::Stmt::Block { statements, .. } => statements
            .iter()
            .find_map(|stmt| match &body.stmts[*stmt] {
                nova_hir::hir::Stmt::Return {
                    expr: Some(expr), ..
                } => Some(*expr),
                _ => None,
            })
            .expect("expected a return expression"),
        other => panic!("expected a block root statement, got {other:?}"),
    };

    let project = db.file_project(file);
    let base_store = db.project_base_type_store(project);

    db.clear_query_stats();

    let ty = db.type_of_expr(
        file,
        FileExprId {
            owner,
            expr: return_expr,
        },
    );
    assert_eq!(
        format_type(&*base_store, &ty),
        "String",
        "expected type_of_expr to resolve the substring call return type"
    );

    let stats = db.query_stats();
    assert!(
        stats.by_query.get("typeck_body").is_none(),
        "type_of_expr should not execute typeck_body; stats: {:?}",
        stats.by_query.get("typeck_body")
    );
}

#[test]
fn demand_type_of_expr_does_not_report_unrelated_type_mismatch_diagnostics() {
    let src = r#"
class C {
    String m() {
        int y = "no";
        return "x".substring(1);
    }
}
"#;

    let (db, file) = setup_db(src);

    let tree = db.hir_item_tree(file);
    let method_ast = tree
        .methods
        .iter()
        .find_map(|(ast_id, m)| (m.name == "m" && m.body.is_some()).then_some(*ast_id))
        .expect("expected method `m` with a body");
    let method_id = nova_hir::ids::MethodId::new(file, method_ast);
    let owner = DefWithBodyId::Method(method_id);

    let body = db.hir_body(method_id);
    let root = &body.stmts[body.root];
    let return_expr = match root {
        nova_hir::hir::Stmt::Block { statements, .. } => statements
            .iter()
            .find_map(|stmt| match &body.stmts[*stmt] {
                nova_hir::hir::Stmt::Return {
                    expr: Some(expr), ..
                } => Some(*expr),
                _ => None,
            })
            .expect("expected a return expression"),
        other => panic!("expected a block root statement, got {other:?}"),
    };

    db.clear_query_stats();
    let demand_res = db.type_of_expr_demand_result(
        file,
        FileExprId {
            owner,
            expr: return_expr,
        },
    );
    assert_eq!(
        format_type(&*demand_res.env, &demand_res.ty),
        "String",
        "expected demand-driven inference to resolve the substring call return type"
    );
    assert!(
        demand_res
            .diagnostics
            .iter()
            .all(|d| d.code.as_ref() != "type-mismatch"),
        "expected no type-mismatch diagnostics from demand-driven query; got {:?}",
        demand_res.diagnostics
    );

    let stats = db.query_stats();
    assert!(
        stats.by_query.get("typeck_body").is_none(),
        "type_of_expr_demand_result should not execute typeck_body; stats: {:?}",
        stats.by_query.get("typeck_body")
    );

    // Full diagnostics should still report the unrelated type mismatch.
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "type-mismatch"),
        "expected full type checking to report a type-mismatch diagnostic; got {diags:?}"
    );
}

#[test]
fn demand_reports_unresolved_type_for_catch_var() {
    let src = r#"
class C {
    void m() {
        try { }
        catch (var e) {
            e.toString();
        }
    }
}
"#;

    let (db, file) = setup_db(src);

    let tree = db.hir_item_tree(file);
    let method_ast = tree
        .methods
        .iter()
        .find_map(|(ast_id, m)| (m.name == "m" && m.body.is_some()).then_some(*ast_id))
        .expect("expected method `m` with a body");
    let method_id = nova_hir::ids::MethodId::new(file, method_ast);
    let owner = DefWithBodyId::Method(method_id);

    let body = db.hir_body(method_id);
    let try_stmt = match &body.stmts[body.root] {
        nova_hir::hir::Stmt::Block { statements, .. } => statements
            .iter()
            .find_map(|stmt| match &body.stmts[*stmt] {
                nova_hir::hir::Stmt::Try { .. } => Some(*stmt),
                _ => None,
            })
            .expect("expected a try statement"),
        other => panic!("expected a block root statement, got {other:?}"),
    };

    let catch_body = match &body.stmts[try_stmt] {
        nova_hir::hir::Stmt::Try { catches, .. } => catches
            .first()
            .map(|c| c.body)
            .expect("expected at least one catch clause"),
        other => panic!("expected a try statement, got {other:?}"),
    };

    let call_expr = match &body.stmts[catch_body] {
        nova_hir::hir::Stmt::Block { statements, .. } => statements
            .iter()
            .find_map(|stmt| match &body.stmts[*stmt] {
                nova_hir::hir::Stmt::Expr { expr, .. } => Some(*expr),
                _ => None,
            })
            .expect("expected an expression statement in catch body"),
        other => panic!("expected catch body to be a block, got {other:?}"),
    };

    let res = db.type_of_expr_demand_result(
        file,
        FileExprId {
            owner,
            expr: call_expr,
        },
    );

    assert!(
        res.diagnostics
            .iter()
            .any(|d| d.code.as_ref() == "unresolved-type" && d.message.contains("var")),
        "expected demand-driven type inference to report unresolved-type for `catch (var e)`; got {:?}",
        res.diagnostics
    );
}

#[test]
fn demand_reports_var_poly_expression_for_local_var_initializer() {
    let src = r#"
class C {
    void m() {
        var f = (s) -> s;
        f.toString();
    }
}
"#;

    let (db, file) = setup_db(src);

    let tree = db.hir_item_tree(file);
    let method_ast = tree
        .methods
        .iter()
        .find_map(|(ast_id, m)| (m.name == "m" && m.body.is_some()).then_some(*ast_id))
        .expect("expected method `m` with a body");
    let method_id = nova_hir::ids::MethodId::new(file, method_ast);
    let owner = DefWithBodyId::Method(method_id);

    let body = db.hir_body(method_id);
    let call_expr = match &body.stmts[body.root] {
        nova_hir::hir::Stmt::Block { statements, .. } => statements
            .iter()
            .find_map(|stmt| match &body.stmts[*stmt] {
                nova_hir::hir::Stmt::Expr { expr, .. } => Some(*expr),
                _ => None,
            })
            .expect("expected an expression statement"),
        other => panic!("expected a block root statement, got {other:?}"),
    };

    let res = db.type_of_expr_demand_result(
        file,
        FileExprId {
            owner,
            expr: call_expr,
        },
    );

    assert!(
        res.diagnostics
            .iter()
            .any(|d| d.code.as_ref() == "var-poly-expression"),
        "expected demand-driven type inference to report var-poly-expression for `var f = (s) -> s;`, got {:?}",
        res.diagnostics
    );
}

#[test]
fn demand_reports_var_void_initializer_for_local_var_initializer() {
    let src = r#"
class C {
    void foo() {}
    void m() {
        var x = foo();
        x.toString();
    }
}
"#;

    let (db, file) = setup_db(src);

    let tree = db.hir_item_tree(file);
    let method_ast = tree
        .methods
        .iter()
        .find_map(|(ast_id, m)| (m.name == "m" && m.body.is_some()).then_some(*ast_id))
        .expect("expected method `m` with a body");
    let method_id = nova_hir::ids::MethodId::new(file, method_ast);
    let owner = DefWithBodyId::Method(method_id);

    let body = db.hir_body(method_id);
    let call_expr = match &body.stmts[body.root] {
        nova_hir::hir::Stmt::Block { statements, .. } => statements
            .iter()
            .find_map(|stmt| match &body.stmts[*stmt] {
                nova_hir::hir::Stmt::Expr { expr, .. } => Some(*expr),
                _ => None,
            })
            .expect("expected an expression statement"),
        other => panic!("expected a block root statement, got {other:?}"),
    };

    let res = db.type_of_expr_demand_result(
        file,
        FileExprId {
            owner,
            expr: call_expr,
        },
    );

    assert!(
        res.diagnostics
            .iter()
            .any(|d| d.code.as_ref() == "var-void-initializer"),
        "expected demand-driven type inference to report var-void-initializer for `var x = foo();`, got {:?}",
        res.diagnostics
    );
}

#[test]
fn demand_reports_var_cannot_infer_for_error_initializer() {
    let src = r#"
class C {
    void m() {
        var x = doesNotExist();
        x.toString();
    }
}
"#;

    let (db, file) = setup_db(src);

    let tree = db.hir_item_tree(file);
    let method_ast = tree
        .methods
        .iter()
        .find_map(|(ast_id, m)| (m.name == "m" && m.body.is_some()).then_some(*ast_id))
        .expect("expected method `m` with a body");
    let method_id = nova_hir::ids::MethodId::new(file, method_ast);
    let owner = DefWithBodyId::Method(method_id);

    let body = db.hir_body(method_id);
    let call_expr = match &body.stmts[body.root] {
        nova_hir::hir::Stmt::Block { statements, .. } => statements
            .iter()
            .find_map(|stmt| match &body.stmts[*stmt] {
                nova_hir::hir::Stmt::Expr { expr, .. } => Some(*expr),
                _ => None,
            })
            .expect("expected an expression statement"),
        other => panic!("expected a block root statement, got {other:?}"),
    };

    let res = db.type_of_expr_demand_result(
        file,
        FileExprId {
            owner,
            expr: call_expr,
        },
    );

    assert!(
        res.diagnostics
            .iter()
            .any(|d| d.code.as_ref() == "var-cannot-infer"),
        "expected demand-driven type inference to report var-cannot-infer for `var x = doesNotExist();`, got {:?}",
        res.diagnostics
    );
}

#[test]
fn demand_type_of_expr_sees_cast_target_type_for_lambda() {
    let src = r#"
class C {
    void m() {
        Runnable r = (Runnable) () -> {};
    }
}
"#;

    let (db, file) = setup_db(src);

    let tree = db.hir_item_tree(file);
    let method_ast = tree
        .methods
        .iter()
        .find_map(|(ast_id, m)| (m.name == "m" && m.body.is_some()).then_some(*ast_id))
        .expect("expected method `m` with a body");
    let method_id = nova_hir::ids::MethodId::new(file, method_ast);
    let owner = DefWithBodyId::Method(method_id);

    let body = db.hir_body(method_id);
    let init_expr = match &body.stmts[body.root] {
        nova_hir::hir::Stmt::Block { statements, .. } => statements
            .iter()
            .find_map(|stmt| match &body.stmts[*stmt] {
                nova_hir::hir::Stmt::Let {
                    initializer: Some(init),
                    ..
                } => Some(*init),
                _ => None,
            })
            .expect("expected a let statement with an initializer"),
        other => panic!("expected a block root statement, got {other:?}"),
    };

    let lambda_expr = match &body.exprs[init_expr] {
        nova_hir::hir::Expr::Cast { expr, .. } => *expr,
        other => panic!("expected initializer to be a Cast expression, got {other:?}"),
    };
    assert!(
        matches!(&body.exprs[lambda_expr], nova_hir::hir::Expr::Lambda { .. }),
        "expected cast to wrap a Lambda expression"
    );

    // Reset query stats so the assertion below only reflects the `type_of_expr_demand_result` call.
    db.clear_query_stats();

    let res = db.type_of_expr_demand_result(
        file,
        FileExprId {
            owner,
            expr: lambda_expr,
        },
    );

    assert_eq!(
        format_type(&*res.env, &res.ty),
        "Runnable",
        "expected demand-driven inference to use the cast target type for the lambda"
    );

    let typeck_body_executions = db
        .query_stats()
        .by_query
        .get("typeck_body")
        .map(|s| s.executions)
        .unwrap_or(0);
    assert_eq!(
        typeck_body_executions, 0,
        "type_of_expr_demand_result should not execute typeck_body"
    );
}
