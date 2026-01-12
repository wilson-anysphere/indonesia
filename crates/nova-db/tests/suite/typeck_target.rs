//! Minimal `nova-db` type-checking integration test module.
//!
//! `nova-db` integration tests are consolidated into `tests/harness.rs` for compile-time and memory
//! efficiency.
//!
//! This file is used by the `nova-db` `typeck` integration test target, so you can run it with:
//! `bash scripts/cargo_agent.sh test -p nova-db --test typeck`.
//!
//! It can also be run via the consolidated harness + filter:
//! `bash scripts/cargo_agent.sh test --locked -p nova-db --test harness suite::typeck_target`.

use std::path::PathBuf;
use std::sync::Arc;

use nova_db::{ArcEq, FileId, NovaInputs, NovaTypeck, ProjectId, SalsaRootDatabase, SourceRootId};
use nova_jdk::JdkIndex;
use nova_project::{BuildSystem, JavaConfig, JavaVersion, Module, ProjectConfig};
use tempfile::TempDir;

fn base_project_config(root: PathBuf) -> ProjectConfig {
    ProjectConfig {
        workspace_root: root.clone(),
        build_system: BuildSystem::Simple,
        // Make the language level deterministic in tests; don't rely on `JavaConfig::default()`.
        java: JavaConfig {
            source: JavaVersion::JAVA_17,
            target: JavaVersion::JAVA_17,
            enable_preview: false,
        },
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
    db.set_file_text(file, text);
}

fn setup_db(text: &str) -> (SalsaRootDatabase, FileId) {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    let cfg = base_project_config(tmp.path().to_path_buf());
    db.set_project_config(project, Arc::new(cfg));
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let file = FileId::from_raw(1);
    set_file(&mut db, project, file, "src/Test.java", text);
    db.set_project_files(project, Arc::new(vec![file]));

    (db, file)
}

#[test]
fn type_at_offset_shows_long_for_long_literal() {
    let src = r#"
class C { long m(){ return 1L; } }
"#;

    let (db, file) = setup_db(src);
    let offset = src.find("1L").expect("snippet should contain long literal");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "long");
}

#[test]
fn type_at_offset_shows_float_for_float_literal() {
    let src = r#"
class C { float m(){ return 1.0f; } }
"#;

    let (db, file) = setup_db(src);
    let offset = src
        .find("1.0f")
        .expect("snippet should contain float literal");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "float");
}

#[test]
fn type_at_offset_shows_double_for_double_literal() {
    let src = r#"
class C { double m(){ return 1.0; } }
"#;

    let (db, file) = setup_db(src);
    let offset = src.find("1.0").expect("snippet should contain double literal");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "double");
}

#[test]
fn type_at_offset_shows_char_for_char_literal() {
    let src = r#"
class C { char m(){ return 'a'; } }
"#;

    let (db, file) = setup_db(src);
    let offset = src
        .find("'a'")
        .expect("snippet should contain char literal")
        + 1;
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "char");
}

#[test]
fn byte_initializer_allows_char_constant_narrowing() {
    let src = r#"
class C { void m(){ byte b = 'a'; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "type-mismatch"),
        "expected `byte b = 'a'` to type-check via constant narrowing; got {diags:?}"
    );
}

#[test]
fn text_block_is_string() {
    let src = r#"
class C { String m(){ return """x"""; } }
"#;

    let (db, file) = setup_db(src);
    let offset = src
        .find("\"\"\"x\"\"\"")
        .expect("snippet should contain text block literal");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "String");
}

#[test]
fn array_initializer_in_var_decl_typechecks() {
    let src = r#"
class C { void m(){ int[] a = {1,2}; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| {
            d.code.as_ref() != "type-mismatch"
                && d.code.as_ref() != "invalid-array-initializer"
                && d.code.as_ref() != "array-initializer-type-mismatch"
        }),
        "expected array initializer to type-check; got {diags:?}"
    );

    let offset = src.find("{1,2}").expect("snippet should contain array initializer");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "int[]");
}

#[test]
fn array_creation_with_initializer_has_array_type() {
    let src = r#"
class C { void m(){ int[] a = new int[] {1,2}; } }
"#;

    let (db, file) = setup_db(src);

    let offset = src
        .find("new int[] {1,2}")
        .expect("snippet should contain array creation")
        + "new ".len();
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "int[]");
}

#[test]
fn array_initializer_items_target_type_lambdas() {
    let src = r#"
class C { void m(){ Runnable[] rs = { () -> {}, () -> {} }; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| {
            d.code.as_ref() != "type-mismatch"
                && d.code.as_ref() != "invalid-array-initializer"
                && d.code.as_ref() != "lambda-arity-mismatch"
        }),
        "expected lambda array initializer to type-check; got {diags:?}"
    );

    let offset = src
        .find("() -> {}")
        .expect("snippet should contain lambda expression");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "Runnable");
}

#[test]
fn throw_requires_throwable() {
    let src = r#"
class C {
    void m() {
        throw 1;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "invalid-throw"),
        "expected invalid-throw diagnostic; got {diags:?}"
    );
}

#[test]
fn catch_param_requires_throwable_subtype() {
    let src = r#"
class C {
    void m() {
        try { } catch (int e) { }
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "invalid-catch-type"),
        "expected invalid-catch-type diagnostic; got {diags:?}"
    );
}

#[test]
fn catch_exception_is_allowed() {
    let src = r#"
class C {
    void m() {
        try { } catch (Exception e) { }
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "invalid-catch-type"),
        "expected no invalid-catch-type diagnostic; got {diags:?}"
    );
    assert!(
        diags
            .iter()
            .all(|d| !(d.code.as_ref() == "unresolved-type" && d.message.contains("Exception"))),
        "expected Exception to resolve from built-in JDK index; got {diags:?}"
    );
}

#[test]
fn varargs_method_call_resolves() {
    let src = r#"
class C {
    static void foo(int... xs) {}
    static void m() { foo(1, 2, 3); }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected varargs call to resolve, got {diags:?}"
    );
}

#[test]
fn instanceof_expression_typechecks_as_boolean() {
    let src = r#"
class C {
    void m() {
        boolean b = "x" instanceof String;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(diags.is_empty(), "expected no diagnostics, got {diags:?}");

    let offset = src.find("instanceof").expect("snippet should contain instanceof");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "boolean");
}

#[test]
fn instanceof_reports_primitive_lhs() {
    let src = r#"
class C {
    void m() {
        boolean b = 1 instanceof String;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "instanceof-primitive"),
        "expected instanceof-primitive diagnostic, got {diags:?}"
    );

    let offset = src.find("instanceof").expect("snippet should contain instanceof");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "boolean");
}

#[test]
fn instanceof_reports_void_rhs() {
    let src = r#"
class C {
    void m() {
        boolean b = "x" instanceof void;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "instanceof-void"),
        "expected instanceof-void diagnostic, got {diags:?}"
    );

    let offset = src.find("instanceof").expect("snippet should contain instanceof");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "boolean");
}

#[test]
fn synchronized_on_primitive_is_error() {
    let src = r#"
class C {
    void m() {
        int x = 0;
        synchronized (x) { }
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "invalid-synchronized-expression"),
        "expected invalid-synchronized-expression diagnostic, got {diags:?}"
    );
}

#[test]
fn synchronized_on_reference_is_ok() {
    let src = r#"
class C {
    void m() {
        Object x = new Object();
        synchronized (x) { }
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_ref() == "invalid-synchronized-expression"),
        "did not expect invalid-synchronized-expression diagnostic, got {diags:?}"
    );
}

#[test]
fn while_condition_must_be_boolean() {
    let src = r#"
class C { void m(){ while (1) {} } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "condition-not-boolean"),
        "expected condition-not-boolean diagnostic; got {diags:?}"
    );
}

#[test]
fn for_condition_must_be_boolean() {
    let src = r#"
class C { void m(){ for (; 1; ) {} } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "condition-not-boolean"),
        "expected condition-not-boolean diagnostic; got {diags:?}"
    );
}

#[test]
fn return_without_value_in_nonvoid_is_error() {
    let src = r#"
class C {
    String m() { return; }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "return-mismatch"),
        "expected return-mismatch diagnostic; got {diags:?}"
    );
}

#[test]
fn byte_initializer_allows_int_constant_narrowing() {
    let src = r#"
class C { void m(){ byte b = 1; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "type-mismatch"),
        "expected constant narrowing to avoid type-mismatch; got {diags:?}"
    );
}

#[test]
fn byte_initializer_allows_shift_constant_narrowing() {
    let src = r#"
class C { void m(){ byte b = 1 << 2; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "type-mismatch"),
        "expected shift constant to narrow to byte; got {diags:?}"
    );
}

#[test]
fn byte_initializer_rejects_out_of_range_shift_constant() {
    let src = r#"
class C { void m(){ byte b = 1 << 10; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "type-mismatch"),
        "expected out-of-range shift constant to produce type-mismatch; got {diags:?}"
    );
}

#[test]
fn comparison_expression_types_as_boolean() {
    let src = r#"
class C { void m(){ boolean b = 1 < 2; } }
"#;

    let (db, file) = setup_db(src);
    let offset = src.find('<').expect("snippet should contain <");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "boolean");
}

#[test]
fn this_and_super_in_static_context_emit_diagnostics() {
    let src = r#"
class C {
    static void m() {
        this.toString();
        super.toString();
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "this-in-static-context"),
        "expected this-in-static-context diagnostic; got {diags:?}"
    );
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "super-in-static-context"),
        "expected super-in-static-context diagnostic; got {diags:?}"
    );
}
