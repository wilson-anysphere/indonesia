use std::sync::Arc;

use std::path::PathBuf;

use nova_db::{ArcEq, FileId, NovaInputs, NovaTypeck, ProjectId, SalsaRootDatabase, SourceRootId};
use nova_jdk::JdkIndex;
use nova_project::{BuildSystem, JavaConfig, Module, ProjectConfig};
use tempfile::TempDir;

fn base_project_config(root: PathBuf) -> ProjectConfig {
    ProjectConfig {
        workspace_root: root.clone(),
        build_system: BuildSystem::Simple,
        java: JavaConfig::default(),
        modules: vec![Module {
            name: "dummy".to_string(),
            root,
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

fn set_file(
    db: &mut SalsaRootDatabase,
    project: ProjectId,
    file: FileId,
    rel_path: &str,
    text: &str,
) {
    db.set_file_project(file, project);
    db.set_file_rel_path(file, Arc::new(rel_path.to_string()));
    db.set_source_root(file, SourceRootId::from_raw(0));
    db.set_file_exists(file, true);
    db.set_file_content(file, Arc::new(text.to_string()));
}

fn setup_db(text: &str) -> (SalsaRootDatabase, FileId) {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();
    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let file = FileId::from_raw(1);
    set_file(&mut db, project, file, "src/Test.java", text);
    db.set_project_files(project, Arc::new(vec![file]));
    (db, file)
}

#[test]
fn reports_type_mismatch_for_bad_initializer() {
    let src = r#"
class C {
    void m() {
        int x = "no";
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    let mismatch = diags
        .iter()
        .find(|d| d.code.as_ref() == "type-mismatch")
        .expect("expected type-mismatch diagnostic");

    let span = mismatch
        .span
        .expect("type-mismatch diagnostic should have a span");
    let quote = src
        .find("\"no\"")
        .expect("snippet should contain string literal");
    assert!(
        span.start <= quote && quote < span.end,
        "expected diagnostic span to cover string literal, got {span:?}"
    );
}

#[test]
fn reports_type_mismatch_for_bad_assignment() {
    let src = r#"
class C {
    void m() {
        int x = 0;
        x = "no";
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "type-mismatch"),
        "expected type-mismatch diagnostic, got {diags:?}"
    );
}

#[test]
fn reports_condition_not_boolean_for_if() {
    let src = r#"
class C {
    void m() {
        if (1) {}
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "condition-not-boolean"),
        "expected condition-not-boolean diagnostic, got {diags:?}"
    );
}

#[test]
fn type_at_offset_shows_string_for_substring_call() {
    let src = r#"
class C {
    String m() {
        return "x".substring(1);
    }
}
"#;

    let (db, file) = setup_db(src);
    let offset = src
        .find("substring(")
        .expect("snippet should contain substring call")
        + "substring".len();
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "String");
}

#[test]
fn type_at_offset_shows_string_for_concat() {
    let src = r#"
class C {
    String m() {
        return "a" + 1;
    }
}
"#;

    let (db, file) = setup_db(src);
    let offset = src.find('+').expect("snippet should contain +");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "String");
}

#[test]
fn unqualified_method_call_resolves_against_enclosing_class() {
    let src = r#"
class C {
    void bar() {}
    void m() {
        bar();
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected unqualified method call to resolve via implicit receiver, got {diags:?}"
    );
}

#[test]
fn static_context_rejects_unqualified_instance_method_call() {
    let src = r#"
class C {
    void bar() {}
    static void m() {
        bar();
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "unresolved-method"
            && d.message.contains("static context")),
        "expected static context to reject implicit-this call, got {diags:?}"
    );
}

#[test]
fn type_at_offset_shows_enclosing_class_for_this() {
    let src = r#"
class C {
    void m() {
        Object o = this;
    }
}
"#;

    let (db, file) = setup_db(src);
    let offset = src.find("this").expect("snippet should contain `this`") + 1;
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "C");
}

#[test]
fn type_at_offset_shows_object_for_super() {
    let src = r#"
class C {
    void m() {
        super.toString();
    }
}
"#;

    let (db, file) = setup_db(src);
    let offset = src.find("super").expect("snippet should contain `super`") + 1;
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "Object");
}

#[test]
fn cross_file_type_reference_resolves_in_same_package() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let a_file = FileId::from_raw(1);
    let b_file = FileId::from_raw(2);

    set_file(
        &mut db,
        project,
        a_file,
        "src/p/A.java",
        "package p; class A { static int F = 1; }",
    );
    set_file(
        &mut db,
        project,
        b_file,
        "src/p/B.java",
        "package p; class B { int x = A.F; }",
    );
    db.set_project_files(project, Arc::new(vec![a_file, b_file]));

    let diags = db.type_diagnostics(b_file);
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_ref() == "unresolved-name" && d.message.contains("`A`")),
        "expected `A` to resolve via workspace, got {diags:?}"
    );
}

#[test]
fn differential_javac_type_mismatch() {
    use nova_test_utils::javac::{javac_available, run_javac_snippet};

    if !javac_available() {
        eprintln!("skipping: javac not available");
        return;
    }

    let src = r#"
class Test {
    void m() {
        int x = "no";
    }
}
"#;

    let out = run_javac_snippet(src).expect("failed to invoke javac");
    assert!(!out.success(), "expected javac to reject the snippet");

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "type-mismatch"),
        "expected Nova to report a type-mismatch diagnostic; got {diags:?}"
    );
}

#[test]
fn unresolved_signature_types_are_anchored() {
    let src = r#"
class C {
    DoesNotExist id(AlsoMissing x) { return null; }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);

    let unresolved: Vec<_> = diags
        .iter()
        .filter(|d| d.code.as_ref() == "unresolved-type")
        .collect();
    assert!(
        unresolved.len() >= 2,
        "expected at least two unresolved-type diagnostics, got {diags:?}"
    );

    for diag in unresolved {
        let span = diag
            .span
            .expect("unresolved-type diagnostic should have a span");
        let snippet = &src[span.start..span.end];
        assert!(
            snippet == "DoesNotExist" || snippet == "AlsoMissing",
            "expected span to cover the unresolved type name, got {snippet:?} for {span:?}"
        );
    }
}
