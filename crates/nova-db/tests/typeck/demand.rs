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
    db.set_file_content(file, Arc::new(text.to_string()));

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
