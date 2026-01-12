use std::sync::Arc;

use nova_db::salsa::{NovaFlow, NovaHir, NovaResolve, NovaSyntax, NovaTypeck};
use nova_db::{ArcEq, FileId, NovaInputs, ProjectId, SalsaRootDatabase, SourceRootId};
use nova_jdk::JdkIndex;
use nova_project::{BuildSystem, JavaConfig, Module, ProjectConfig};
use tempfile::TempDir;

fn base_project_config(root: &TempDir) -> ProjectConfig {
    ProjectConfig {
        workspace_root: root.path().to_path_buf(),
        build_system: BuildSystem::Simple,
        java: JavaConfig::default(),
        modules: vec![Module {
            name: "dummy".to_string(),
            root: root.path().to_path_buf(),
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
    }
}

fn setup_db(text: &str) -> (SalsaRootDatabase, TempDir, FileId) {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().expect("temp dir");

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);
    db.set_project_config(project, Arc::new(base_project_config(&tmp)));

    let file = FileId::from_raw(1);
    db.set_file_project(file, project);
    let rel_path = Arc::new("src/C.java".to_string());
    db.set_file_rel_path(file, Arc::clone(&rel_path));
    db.set_file_path_arc(file, rel_path);
    db.set_source_root(file, SourceRootId::from_raw(0));
    db.set_file_text(file, text);
    db.set_all_file_ids(Arc::new(vec![file]));
    db.set_project_files(project, Arc::new(vec![file]));

    (db, tmp, file)
}

fn executions(db: &SalsaRootDatabase, query_name: &str) -> u64 {
    db.query_stats()
        .by_query
        .get(query_name)
        .map(|s| s.executions)
        .unwrap_or(0)
}

#[test]
fn semantic_queries_do_not_panic_on_missing_initializer_expr() {
    let src = r#"
class C {
    void m() {
        int x = ;
        if (true) { }
    }
}
"#;

    let (db, _tmp, file) = setup_db(src);

    // Ensure this is actually syntactically malformed so the test exercises recovery paths.
    assert!(
        !db.parse_java(file).errors.is_empty(),
        "expected parse errors for malformed snippet"
    );

    let tree = db.hir_item_tree(file);
    assert!(
        tree.methods
            .values()
            .any(|m| m.name == "m" && m.body.is_some()),
        "expected a method body to be present so typeck/flow is exercised"
    );

    let _ = db.scope_graph(file);

    let _ = db.type_diagnostics(file);
    assert!(
        executions(&db, "typeck_body") > 0,
        "expected type checking to run for at least one body"
    );

    let _ = db.flow_diagnostics_for_file(file);
    assert!(
        executions(&db, "flow_diagnostics") > 0,
        "expected flow diagnostics to run for at least one method"
    );
}

#[test]
fn semantic_queries_do_not_panic_on_missing_switch_braces() {
    let src = r#"
class C {
    void m(int x) {
        switch (x)
            case 1: break;
    }
}
"#;

    let (db, _tmp, file) = setup_db(src);

    assert!(
        !db.parse_java(file).errors.is_empty(),
        "expected parse errors for malformed snippet"
    );

    let tree = db.hir_item_tree(file);
    assert!(
        tree.methods
            .values()
            .any(|m| m.name == "m" && m.body.is_some()),
        "expected a method body to be present so typeck/flow is exercised"
    );

    let _ = db.scope_graph(file);

    let _ = db.type_diagnostics(file);
    assert!(
        executions(&db, "typeck_body") > 0,
        "expected type checking to run for at least one body"
    );

    let _ = db.flow_diagnostics_for_file(file);
    assert!(
        executions(&db, "flow_diagnostics") > 0,
        "expected flow diagnostics to run for at least one method"
    );
}

#[test]
fn type_at_offset_display_does_not_panic_on_broken_code() {
    let src = r#"
class C {
    void m() {
        int x = ;
        if (true) { }
    }
}
"#;

    let (db, _tmp, file) = setup_db(src);

    // Ensure this is actually syntactically malformed so the test exercises recovery paths.
    assert!(
        !db.parse_java(file).errors.is_empty(),
        "expected parse errors for malformed snippet"
    );

    let offset = src
        .find("true")
        .expect("snippet should contain boolean literal");

    // Ensure this IDE query stays demand-driven even on broken code.
    db.clear_query_stats();
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "boolean");

    let typeck_body_executions = db
        .query_stats()
        .by_query
        .get("typeck_body")
        .map(|s| s.executions)
        .unwrap_or(0);
    assert_eq!(
        typeck_body_executions, 0,
        "type_at_offset_display should not execute typeck_body on broken code"
    );
}
