use std::sync::Arc;

use nova_db::{
    salsa::FileExprId, ArcEq, FileId, NovaHir, NovaInputs, NovaTypeck, ProjectId,
    SalsaRootDatabase, SourceRootId,
};
use nova_jdk::JdkIndex;
use nova_project::{BuildSystem, JavaConfig, Module, ProjectConfig};
use nova_resolve::ids::DefWithBodyId;
use tempfile::TempDir;

fn setup_db(text: &str) -> (SalsaRootDatabase, FileId) {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_project_config(
        project,
        Arc::new(ProjectConfig {
            workspace_root: tmp.path().to_path_buf(),
            build_system: BuildSystem::Simple,
            java: JavaConfig::default(),
            modules: vec![Module {
                name: "dummy".to_string(),
                root: tmp.path().to_path_buf(),
                annotation_processing: Default::default(),
            }],
            jpms_modules: Vec::new(),
            jpms_workspace: None,
            source_roots: Vec::new(),
            module_path: Vec::new(),
            classpath: Vec::new(),
            output_dirs: Vec::new(),
            dependencies: Vec::new(),
            workspace_model: None,
        }),
    );
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let file = FileId::from_raw(1);
    db.set_file_project(file, project);
    db.set_file_rel_path(file, Arc::new("src/Test.java".to_string()));
    db.set_source_root(file, SourceRootId::from_raw(0));
    db.set_file_text(file, text);
    db.set_project_files(project, Arc::new(vec![file]));
    (db, file)
}

fn executions(db: &SalsaRootDatabase, query_name: &str) -> u64 {
    db.query_stats()
        .by_query
        .get(query_name)
        .map(|s| s.executions)
        .unwrap_or(0)
}

#[test]
fn resolve_method_call_is_demand_driven() {
    let src = r#"
class C {
    String m() {
        return "x".substring(1);
    }
}
"#;

    let (db, file) = setup_db(src);

    // Locate the call expression id inside `C.m`.
    let tree = db.hir_item_tree(file);
    let (&m_ast_id, _) = tree
        .methods
        .iter()
        .find(|(_, method)| method.name == "m")
        .expect("expected C.m method");
    let m_id = nova_hir::ids::MethodId::new(file, m_ast_id);
    let body = db.hir_body(m_id);

    let call_expr = match &body.stmts[body.root] {
        nova_hir::hir::Stmt::Block { statements, .. } => statements
            .iter()
            .find_map(|stmt| match &body.stmts[*stmt] {
                nova_hir::hir::Stmt::Return {
                    expr: Some(expr), ..
                } => Some(*expr),
                _ => None,
            })
            .expect("expected return expr"),
        other => panic!("expected root block, got {other:?}"),
    };

    db.clear_query_stats();

    let resolved = db.resolve_method_call(
        file,
        FileExprId {
            owner: DefWithBodyId::Method(m_id),
            expr: call_expr,
        },
    );

    let resolved = resolved.expect("expected method call to resolve");
    assert_eq!(resolved.name, "substring");

    assert_eq!(
        executions(&db, "typeck_body"),
        0,
        "resolve_method_call should not force whole-body typeck"
    );
}

#[test]
fn resolve_method_call_infers_var_local_receiver() {
    let src = r#"
class C {
    String m() {
        var s = "x";
        return s.substring(1);
    }
}
"#;

    let (db, file) = setup_db(src);

    // Locate the call expression id inside `C.m`.
    let tree = db.hir_item_tree(file);
    let (&m_ast_id, _) = tree
        .methods
        .iter()
        .find(|(_, method)| method.name == "m")
        .expect("expected C.m method");
    let m_id = nova_hir::ids::MethodId::new(file, m_ast_id);
    let body = db.hir_body(m_id);

    let call_expr = match &body.stmts[body.root] {
        nova_hir::hir::Stmt::Block { statements, .. } => statements
            .iter()
            .find_map(|stmt| match &body.stmts[*stmt] {
                nova_hir::hir::Stmt::Return {
                    expr: Some(expr), ..
                } => Some(*expr),
                _ => None,
            })
            .expect("expected return expr"),
        other => panic!("expected root block, got {other:?}"),
    };

    db.clear_query_stats();

    let resolved = db.resolve_method_call(
        file,
        FileExprId {
            owner: DefWithBodyId::Method(m_id),
            expr: call_expr,
        },
    );

    let resolved = resolved.expect("expected method call to resolve");
    assert_eq!(resolved.name, "substring");

    assert_eq!(
        executions(&db, "typeck_body"),
        0,
        "resolve_method_call should not force whole-body typeck"
    );
}

#[test]
fn resolve_method_call_returns_none_on_ambiguous_calls() {
    let src = r#"
class C {
    String foo(String x) { return ""; }
    String foo(Integer x) { return ""; }
    String m() { return foo(null); }
}
"#;

    let (db, file) = setup_db(src);

    // Locate the call expression id inside `C.m`.
    let tree = db.hir_item_tree(file);
    let (&m_ast_id, _) = tree
        .methods
        .iter()
        .find(|(_, method)| method.name == "m")
        .expect("expected C.m method");
    let m_id = nova_hir::ids::MethodId::new(file, m_ast_id);
    let body = db.hir_body(m_id);

    let call_expr = match &body.stmts[body.root] {
        nova_hir::hir::Stmt::Block { statements, .. } => statements
            .iter()
            .find_map(|stmt| match &body.stmts[*stmt] {
                nova_hir::hir::Stmt::Return {
                    expr: Some(expr), ..
                } => Some(*expr),
                _ => None,
            })
            .expect("expected return expr"),
        other => panic!("expected root block, got {other:?}"),
    };

    db.clear_query_stats();

    let resolved = db.resolve_method_call(
        file,
        FileExprId {
            owner: DefWithBodyId::Method(m_id),
            expr: call_expr,
        },
    );

    assert!(
        resolved.is_none(),
        "ambiguous call should resolve to None for IDE helpers"
    );
    assert_eq!(
        executions(&db, "typeck_body"),
        0,
        "resolve_method_call should not force whole-body typeck"
    );
}

#[test]
fn resolve_method_call_resolves_constructor_calls() {
    let src = r#"
class C {
    C(int x) {}
}

class D {
    void m() {
        new C(1);
    }
}
"#;

    let (db, file) = setup_db(src);

    // Locate the `new C(1)` expression id inside `D.m`.
    let tree = db.hir_item_tree(file);
    let (&m_ast_id, _) = tree
        .methods
        .iter()
        .find(|(_, method)| method.name == "m")
        .expect("expected D.m method");
    let m_id = nova_hir::ids::MethodId::new(file, m_ast_id);
    let body = db.hir_body(m_id);

    let new_expr = match &body.stmts[body.root] {
        nova_hir::hir::Stmt::Block { statements, .. } => statements
            .iter()
            .find_map(|stmt| match &body.stmts[*stmt] {
                nova_hir::hir::Stmt::Expr { expr, .. } => Some(*expr),
                _ => None,
            })
            .expect("expected expression statement"),
        other => panic!("expected root block, got {other:?}"),
    };

    db.clear_query_stats();

    let resolved = db.resolve_method_call(
        file,
        FileExprId {
            owner: DefWithBodyId::Method(m_id),
            expr: new_expr,
        },
    );

    let resolved = resolved.expect("expected constructor call to resolve");
    assert_eq!(resolved.name, "<init>");
    assert_eq!(
        executions(&db, "typeck_body"),
        0,
        "resolve_method_call should not force whole-body typeck for constructors"
    );
}

#[test]
fn resolve_method_call_returns_none_on_ambiguous_constructor_calls() {
    let src = r#"
class C {
    C(String x) {}
    C(Integer x) {}
}

class D {
    void m() {
        new C(null);
    }
}
"#;

    let (db, file) = setup_db(src);

    // Locate the `new C(null)` expression id inside `D.m`.
    let tree = db.hir_item_tree(file);
    let (&m_ast_id, _) = tree
        .methods
        .iter()
        .find(|(_, method)| method.name == "m")
        .expect("expected D.m method");
    let m_id = nova_hir::ids::MethodId::new(file, m_ast_id);
    let body = db.hir_body(m_id);

    let new_expr = match &body.stmts[body.root] {
        nova_hir::hir::Stmt::Block { statements, .. } => statements
            .iter()
            .find_map(|stmt| match &body.stmts[*stmt] {
                nova_hir::hir::Stmt::Expr { expr, .. } => Some(*expr),
                _ => None,
            })
            .expect("expected expression statement"),
        other => panic!("expected root block, got {other:?}"),
    };

    db.clear_query_stats();

    let resolved = db.resolve_method_call(
        file,
        FileExprId {
            owner: DefWithBodyId::Method(m_id),
            expr: new_expr,
        },
    );

    assert!(
        resolved.is_none(),
        "ambiguous constructor call should resolve to None for IDE helpers"
    );
    assert_eq!(
        executions(&db, "typeck_body"),
        0,
        "resolve_method_call should not force whole-body typeck for constructors"
    );
}
