//! Minimal `nova-db` type-checking integration test module.
//!
//! Most `nova-db` integration tests are consolidated into `tests/harness.rs` for compile-time and
//! memory efficiency (each `tests/*.rs` file is its own integration test binary).
//!
//! Some tooling and older instructions still expect a `typeck` integration test target, so we
//! keep this file around as a small dedicated harness (see `[[test]] name = "typeck"` in
//! `crates/nova-db/Cargo.toml`).
//!
//! ```bash
//! bash scripts/cargo_agent.sh test --locked -p nova-db --test typeck
//! ```
//!
//! It can also be run via the consolidated harness + filter:
//! ```bash
//! bash scripts/cargo_agent.sh test --locked -p nova-db --test harness suite::typeck_target
//! ```

use std::path::PathBuf;
use std::sync::Arc;

use nova_db::{
    ArcEq, FileId, NovaInputs, NovaSyntax, NovaTypeck, ProjectId, SalsaRootDatabase, SourceRootId,
};
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

fn setup_db_with_java(text: &str, source: JavaVersion, enable_preview: bool) -> (SalsaRootDatabase, FileId) {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    let mut cfg = base_project_config(tmp.path().to_path_buf());
    cfg.java.source = source;
    cfg.java.target = source;
    cfg.java.enable_preview = enable_preview;
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
    let offset = src
        .find("1.0")
        .expect("snippet should contain double literal");
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
fn long_initializer_rejects_out_of_range_int_literal() {
    let src = r#"
class C { long m(){ return 2147483648; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "invalid-literal"),
        "expected invalid-literal diagnostic; got {diags:?}"
    );
}

#[test]
fn double_literal_too_large_is_diagnostic() {
    let src = r#"
class C { double m(){ return 1e400; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "invalid-literal"),
        "expected invalid-literal diagnostic; got {diags:?}"
    );
}

#[test]
fn double_literal_too_small_is_diagnostic() {
    let src = r#"
class C { double m(){ return 1e-400; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "invalid-literal"),
        "expected invalid-literal diagnostic; got {diags:?}"
    );
}

#[test]
fn float_literal_too_large_is_diagnostic() {
    let src = r#"
class C { float m(){ return 1e50f; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "invalid-literal"),
        "expected invalid-literal diagnostic; got {diags:?}"
    );
}

#[test]
fn float_literal_too_small_is_diagnostic() {
    let src = r#"
class C { float m(){ return 1e-50f; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "invalid-literal"),
        "expected invalid-literal diagnostic; got {diags:?}"
    );
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
fn switch_expression_has_string_type() {
    let src = r#"
class C {
    String m(int x) {
        String s = switch (x) { case 1 -> "a"; default -> "b"; };
        return s;
    }
}
"#;

    let (db, file) = setup_db(src);
    let offset = src
        .find("switch")
        .expect("snippet should contain switch expression");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "String");

    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "switch-type"),
        "expected switch expression to have inferred type; got {diags:?}"
    );
}

#[test]
fn switch_expression_block_arm_yield_is_typed() {
    let src = r#"
class C {
    String m(int x) {
        return switch (x) {
            case 1 -> { yield "a"; }
            default -> "b";
        };
    }
}
"#;

    let (db, file) = setup_db(src);
    let offset = src
        .find("switch")
        .expect("snippet should contain switch expression");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "String");

    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.code.as_ref() != "yield-outside-switch"),
        "expected yield to be allowed inside switch expression; got {diags:?}"
    );
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "switch-type"),
        "expected switch expression to have inferred type; got {diags:?}"
    );
}

#[test]
fn switch_expression_is_feature_gated_but_typeck_does_not_crash() {
    let src = r#"
class C {
    String m(int x) {
        return switch (x) { case 1 -> "a"; default -> "b"; };
    }
}
"#;

    // Switch expressions are preview in Java 13 and require `--enable-preview`; ensure syntax
    // feature diagnostics still fire while typeck remains resilient.
    let (db, file) = setup_db_with_java(src, JavaVersion(13), false);

    let feature_diags = db.syntax_feature_diagnostics(file);
    assert!(
        feature_diags
            .iter()
            .any(|d| d.code.as_ref() == "JAVA_FEATURE_SWITCH_EXPRESSIONS"),
        "expected JAVA_FEATURE_SWITCH_EXPRESSIONS diagnostic; got {feature_diags:?}"
    );

    // Typeck should still run without panicking and (best-effort) infer a type for IDE features.
    let offset = src
        .find("switch")
        .expect("snippet should contain switch expression");
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

    let offset = src
        .find("{1,2}")
        .expect("snippet should contain array initializer");
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
fn static_context_rejects_unqualified_instance_field_access() {
    let src = r#"
class C {
    int x;
    static void m() {
        x = 1;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "static-context"),
        "expected static-context diagnostic for instance field access in static method; got {diags:?}"
    );
}

#[test]
fn static_context_allows_unqualified_static_field_access() {
    let src = r#"
class C {
    static int x;
    static void m() {
        x = 1;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "static-context"),
        "expected no static-context diagnostic for static field access in static method; got {diags:?}"
    );
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
        diags
            .iter()
            .any(|d| d.code.as_ref() == "invalid-catch-type"),
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
        diags
            .iter()
            .all(|d| d.code.as_ref() != "invalid-catch-type"),
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
fn throw_new_runtime_exception_is_allowed() {
    let src = r#"
class C {
    void m() {
        throw new RuntimeException();
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "invalid-throw"),
        "expected no invalid-throw diagnostic; got {diags:?}"
    );
    assert!(
        diags.iter().all(|d| {
            !(d.code.as_ref() == "unresolved-type" && d.message.contains("RuntimeException"))
        }),
        "expected RuntimeException to resolve from built-in JDK index; got {diags:?}"
    );
    assert!(
        diags
            .iter()
            .all(|d| d.code.as_ref() != "unresolved-constructor"),
        "expected no unresolved-constructor diagnostic; got {diags:?}"
    );
}

#[test]
fn catch_runtime_exception_is_allowed() {
    let src = r#"
class C {
    void m() {
        try { } catch (RuntimeException e) { }
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.code.as_ref() != "invalid-catch-type"),
        "expected no invalid-catch-type diagnostic; got {diags:?}"
    );
    assert!(
        diags.iter().all(|d| {
            !(d.code.as_ref() == "unresolved-type" && d.message.contains("RuntimeException"))
        }),
        "expected RuntimeException to resolve from built-in JDK index; got {diags:?}"
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

    let offset = src
        .find("instanceof")
        .expect("snippet should contain instanceof");
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
        diags
            .iter()
            .any(|d| d.code.as_ref() == "instanceof-primitive"),
        "expected instanceof-primitive diagnostic, got {diags:?}"
    );

    let offset = src
        .find("instanceof")
        .expect("snippet should contain instanceof");
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

    let offset = src
        .find("instanceof")
        .expect("snippet should contain instanceof");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "boolean");
}

#[test]
fn assert_statement_typechecks_for_boolean_condition() {
    let src = r#"
class C {
    void m() {
        assert 1 < 2;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_ref() == "assert-condition-not-boolean"),
        "expected assert statement to type-check, got {diags:?}"
    );
}

#[test]
fn assert_statement_requires_boolean_condition() {
    let src = r#"
class C {
    void m() {
        assert 1;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "assert-condition-not-boolean"),
        "expected assert-condition-not-boolean diagnostic, got {diags:?}"
    );
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
            .any(|d| d.code.as_ref() == "non-reference-monitor"),
        "expected non-reference-monitor diagnostic, got {diags:?}"
    );
}

#[test]
fn synchronized_on_null_is_error() {
    let src = r#"
class C {
    void m() {
        synchronized (null) { }
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
fn synchronized_on_void_is_error() {
    let src = r#"
class C {
    void n() { }

    void m() {
        synchronized (n()) { }
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
        !diags.iter().any(|d| d.code.as_ref() == "non-reference-monitor"),
        "did not expect non-reference-monitor diagnostic, got {diags:?}"
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
        diags
            .iter()
            .any(|d| d.code.as_ref() == "condition-not-boolean"),
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
        diags
            .iter()
            .any(|d| d.code.as_ref() == "condition-not-boolean"),
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

#[test]
fn string_concatenation_has_string_type() {
    let src = r#"
class C { String m(){ return "a" + 1; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "type-mismatch"),
        "expected string concatenation to type-check; got {diags:?}"
    );

    let offset = src.find('+').expect("snippet should contain +");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "String");
}

#[test]
fn conditional_numeric_promotion_infers_long() {
    let src = r#"
class C { long m(){ return true ? 1 : 2L; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.severity != nova_types::Severity::Error),
        "expected no errors; got {diags:?}"
    );

    let offset = src.find('?').expect("snippet should contain ?");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "long");
}

#[test]
fn conditional_reference_lub_infers_object() {
    let src = r#"
class C { Object m(){ return true ? "a" : new Object(); } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.severity != nova_types::Severity::Error),
        "expected no errors; got {diags:?}"
    );

    let offset = src.find('?').expect("snippet should contain ?");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "Object");
}

#[test]
fn logical_and_requires_boolean_operands() {
    let src = r#"
class C { void m(){ boolean b = 1 && 2; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "type-mismatch"),
        "expected type-mismatch diagnostic; got {diags:?}"
    );
}

#[test]
fn bitwise_boolean_has_boolean_type() {
    let src = r#"
class C { void m(){ boolean b = true & false; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.severity != nova_types::Severity::Error),
        "expected no errors; got {diags:?}"
    );

    let offset = src.find('&').expect("snippet should contain &");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "boolean");
}

#[test]
fn shift_expression_with_long_lhs_types_as_long() {
    let src = r#"
class C { long m(){ return 1L << 2; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.severity != nova_types::Severity::Error),
        "expected no errors; got {diags:?}"
    );

    let offset = src.find("<<").expect("snippet should contain <<");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "long");
}

#[test]
fn conditional_with_null_infers_string() {
    let src = r#"
class C { String m(){ return true ? "a" : null; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.severity != nova_types::Severity::Error),
        "expected no errors; got {diags:?}"
    );

    let offset = src.find('?').expect("snippet should contain ?");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "String");
}

#[test]
fn equality_expression_types_as_boolean() {
    let src = r#"
class C { void m(){ boolean b = 1 == 2; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.severity != nova_types::Severity::Error),
        "expected no errors; got {diags:?}"
    );

    let offset = src.find("==").expect("snippet should contain ==");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "boolean");
}

#[test]
fn logical_not_requires_boolean_operand() {
    let src = r#"
class C { void m(){ boolean b = !1; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "invalid-unary-op"),
        "expected invalid-unary-op diagnostic; got {diags:?}"
    );

    let offset = src.find('!').expect("snippet should contain !");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "boolean");
}

#[test]
fn post_increment_expression_has_operand_type() {
    let src = r#"
class C { void m(){ byte b = 0; byte c = b++; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.severity != nova_types::Severity::Error),
        "expected no errors; got {diags:?}"
    );

    let offset = src.find("++").expect("snippet should contain ++");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "byte");
}

#[test]
fn unary_plus_promotes_byte_to_int() {
    let src = r#"
class C { int m(byte b){ return +b; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.severity != nova_types::Severity::Error),
        "expected no errors; got {diags:?}"
    );

    let offset = src.find('+').expect("snippet should contain unary +");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "int");
}

#[test]
fn numeric_addition_promotes_to_long() {
    let src = r#"
class C { long m(){ return 1 + 2L; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.severity != nova_types::Severity::Error),
        "expected no errors; got {diags:?}"
    );

    let offset = src.find('+').expect("snippet should contain +");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "long");
}

#[test]
fn bitwise_or_on_bytes_promotes_to_int() {
    let src = r#"
class C { int m(byte a, byte b){ return a | b; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.severity != nova_types::Severity::Error),
        "expected no errors; got {diags:?}"
    );

    let offset = src.find('|').expect("snippet should contain |");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "int");
}

#[test]
fn shift_expression_with_byte_lhs_types_as_int() {
    let src = r#"
class C { int m(byte a){ return a << 1; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.severity != nova_types::Severity::Error),
        "expected no errors; got {diags:?}"
    );

    let offset = src.find("<<").expect("snippet should contain <<");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "int");
}

#[test]
fn logical_or_types_as_boolean() {
    let src = r#"
class C { void m(){ boolean b = true || false; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.severity != nova_types::Severity::Error),
        "expected no errors; got {diags:?}"
    );

    let offset = src.find("||").expect("snippet should contain ||");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "boolean");
}

#[test]
fn logical_or_requires_boolean_operands() {
    let src = r#"
class C { void m(){ boolean b = 1 || 2; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "type-mismatch"),
        "expected type-mismatch diagnostic; got {diags:?}"
    );
}

#[test]
fn bitwise_or_boolean_has_boolean_type() {
    let src = r#"
class C { void m(){ boolean b = true | false; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.severity != nova_types::Severity::Error),
        "expected no errors; got {diags:?}"
    );

    let offset = src.find('|').expect("snippet should contain |");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "boolean");
}

#[test]
fn bitwise_xor_boolean_has_boolean_type() {
    let src = r#"
class C { void m(){ boolean b = true ^ false; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.severity != nova_types::Severity::Error),
        "expected no errors; got {diags:?}"
    );

    let offset = src.find('^').expect("snippet should contain ^");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "boolean");
}

#[test]
fn bitwise_or_promotes_to_long() {
    let src = r#"
class C { long m(){ return 1L | 2; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.severity != nova_types::Severity::Error),
        "expected no errors; got {diags:?}"
    );

    let offset = src.find('|').expect("snippet should contain |");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "long");
}

#[test]
fn unsigned_shift_expression_types_as_int() {
    let src = r#"
class C { int m(){ return 1 >>> 1; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.severity != nova_types::Severity::Error),
        "expected no errors; got {diags:?}"
    );

    let offset = src.find(">>>").expect("snippet should contain >>>");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "int");
}

#[test]
fn unsigned_shift_expression_with_long_lhs_types_as_long() {
    let src = r#"
class C { long m(){ return 1L >>> 1; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.severity != nova_types::Severity::Error),
        "expected no errors; got {diags:?}"
    );

    let offset = src.find(">>>").expect("snippet should contain >>>");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "long");
}

#[test]
fn shift_expression_with_long_rhs_types_as_int() {
    let src = r#"
class C { int m(){ return 1 << 2L; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.severity != nova_types::Severity::Error),
        "expected no errors; got {diags:?}"
    );

    let offset = src.find("<<").expect("snippet should contain <<");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "int");
}

#[test]
fn string_concatenation_with_null_is_string() {
    let src = r#"
class C { String m(){ return "a" + null; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.severity != nova_types::Severity::Error),
        "expected no errors; got {diags:?}"
    );

    let offset = src.find('+').expect("snippet should contain +");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "String");
}

#[test]
fn system_out_println_resolves_via_minimal_jdk() {
    let src = r#"
class C {
    void m() {
        System.out.println("x");
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(
            |d| d.code.as_ref() != "unresolved-field" && d.code.as_ref() != "unresolved-method"
        ),
        "expected System.out.println to resolve via minimal JDK, got {diags:?}"
    );

    let offset = src
        .find("println(")
        .expect("snippet should contain println call")
        + "println".len();
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "void");
}

#[test]
fn math_max_and_pi_are_typed_via_minimal_jdk() {
    let src = r#"
class C {
    double m() {
        double a = Math.PI;
        int b = Math.max(1, 2);
        return a + b;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| {
            d.code.as_ref() != "unresolved-field"
                && d.code.as_ref() != "unresolved-method"
                && d.code.as_ref() != "unresolved-static-member"
        }),
        "expected Math.PI and Math.max to resolve via minimal JDK, got {diags:?}"
    );

    let pi_offset = src.find("PI").expect("snippet should contain PI");
    let pi_ty = db
        .type_at_offset_display(file, pi_offset as u32)
        .expect("expected a type at offset for PI");
    assert_eq!(pi_ty, "double");

    let max_offset = src.find("max(").expect("snippet should contain max call") + "max".len();
    let max_ty = db
        .type_at_offset_display(file, max_offset as u32)
        .expect("expected a type at offset for max call");
    assert_eq!(max_ty, "int");
}

#[test]
fn assignment_expression_has_lhs_type() {
    let src = r#"
class C { long m(){ long x = 0; return x = 1; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.severity != nova_types::Severity::Error),
        "expected no errors; got {diags:?}"
    );

    let offset = src
        .find("x = 1")
        .expect("snippet should contain assignment expression")
        + "x ".len();
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "long");
}

#[test]
fn assignment_allows_int_constant_narrowing_to_byte() {
    let src = r#"
class C { void m(){ byte b = 0; b = 1; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "type-mismatch"),
        "expected `b = 1` to type-check via constant narrowing; got {diags:?}"
    );
}

#[test]
fn assignment_rejects_out_of_range_int_constant_to_byte() {
    let src = r#"
class C { void m(){ byte b = 0; b = 1000; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "type-mismatch"),
        "expected out-of-range constant assignment to produce type-mismatch; got {diags:?}"
    );
}

#[test]
fn compound_add_assign_expression_has_lhs_type() {
    let src = r#"
class C { void m(){ byte b = 0; byte c = (b += 1); } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.severity != nova_types::Severity::Error),
        "expected no errors; got {diags:?}"
    );

    let offset = src.find("+=").expect("snippet should contain +=");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "byte");
}

#[test]
fn compound_shift_assign_expression_has_lhs_type() {
    let src = r#"
class C { void m(){ byte b = 1; byte c = (b <<= 1); } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.severity != nova_types::Severity::Error),
        "expected no errors; got {diags:?}"
    );

    let offset = src.find("<<=").expect("snippet should contain <<=");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "byte");
}

#[test]
fn boolean_compound_and_assign_expression_has_boolean_type() {
    let src = r#"
class C { void m(){ boolean b = true; boolean c = (b &= false); } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.severity != nova_types::Severity::Error),
        "expected no errors; got {diags:?}"
    );

    let offset = src.find("&=").expect("snippet should contain &=");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "boolean");
}

#[test]
fn conditional_null_and_int_boxes_to_integer() {
    let src = r#"
class C { Integer m(){ return true ? 1 : null; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.severity != nova_types::Severity::Error),
        "expected no errors; got {diags:?}"
    );

    let offset = src.find('?').expect("snippet should contain ?");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "Integer");
}

#[test]
fn array_access_expression_has_element_type() {
    let src = r#"
class C { int m(){ int[] a = {1,2}; return a[0]; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.severity != nova_types::Severity::Error),
        "expected no errors; got {diags:?}"
    );

    let offset = src
        .find("a[0]")
        .expect("snippet should contain array access")
        + 1; // point at `[` so we select the full `a[0]` expression (not `a` or `0`)
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "int");
}

#[test]
fn class_literal_has_class_of_string_type() {
    let src = r#"
class C { Class<String> m(){ return String.class; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.severity != nova_types::Severity::Error),
        "expected no errors; got {diags:?}"
    );

    let offset = src
        .find(".class")
        .expect("snippet should contain class literal");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "Class<String>");
}

#[test]
fn primitive_class_literal_has_class_of_wildcard_type() {
    let src = r#"
class C { Class<?> m(){ return int.class; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.severity != nova_types::Severity::Error),
        "expected no errors; got {diags:?}"
    );

    let offset = src
        .find(".class")
        .expect("snippet should contain primitive class literal");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "Class<?>");
}

#[test]
fn array_class_literal_preserves_array_dims() {
    let src = r#"
class C { Class<String[]> m(){ return String[].class; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.severity != nova_types::Severity::Error),
        "expected no errors; got {diags:?}"
    );

    let offset = src
        .find(".class")
        .expect("snippet should contain array class literal");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "Class<String[]>");
}

#[test]
fn primitive_array_class_literal_has_array_type() {
    let src = r#"
class C { Class<int[]> m(){ return int[].class; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.severity != nova_types::Severity::Error),
        "expected no errors; got {diags:?}"
    );

    let offset = src
        .find(".class")
        .expect("snippet should contain primitive array class literal");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "Class<int[]>");
}

#[test]
fn void_class_literal_has_class_of_wildcard_type() {
    let src = r#"
class C { Class<?> m(){ return void.class; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.severity != nova_types::Severity::Error),
        "expected no errors; got {diags:?}"
    );

    let offset = src
        .find(".class")
        .expect("snippet should contain void class literal");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "Class<?>");
}

#[test]
fn cast_expression_types_as_target_type() {
    let src = r#"
class C {
    String m(Object o) {
        return (String) o;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.severity != nova_types::Severity::Error),
        "expected no errors; got {diags:?}"
    );

    let offset = src
        .find("(String)")
        .expect("snippet should contain cast expression")
        + 1;
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "String");
}

#[test]
fn invalid_cast_emits_invalid_cast_diagnostic() {
    let src = r#"
class C {
    void m() {
        String s = (String) 1;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "invalid-cast"),
        "expected invalid-cast diagnostic; got {diags:?}"
    );
}

#[test]
fn static_method_reference_is_typed_as_target_interface() {
    let src = r#"
interface F { int get(); }

class C {
    static int foo() { return 1; }

    void m() {
        F f = C::foo;
        f.get();
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.severity != nova_types::Severity::Error),
        "expected no errors; got {diags:?}"
    );
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "method-ref-mismatch"),
        "did not expect method-ref-mismatch diagnostic; got {diags:?}"
    );

    let offset = src.find("::").expect("snippet should contain method reference");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "F");
}

#[test]
fn unbound_instance_method_reference_is_typed_as_target_interface() {
    let src = r#"
interface Len { int len(String s); }

class C {
    void m() {
        Len l = String::length;
        l.len("hi");
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.severity != nova_types::Severity::Error),
        "expected no errors; got {diags:?}"
    );
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "method-ref-mismatch"),
        "did not expect method-ref-mismatch diagnostic; got {diags:?}"
    );

    let offset = src.find("::").expect("snippet should contain method reference");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "Len");
}

#[test]
fn bound_instance_method_reference_is_typed_as_target_interface() {
    let src = r#"
interface Int0 { int get(); }

class C {
    void m() {
        String s = "hi";
        Int0 f = s::length;
        f.get();
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.severity != nova_types::Severity::Error),
        "expected no errors; got {diags:?}"
    );
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "method-ref-mismatch"),
        "did not expect method-ref-mismatch diagnostic; got {diags:?}"
    );

    let offset = src.find("::").expect("snippet should contain method reference");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "Int0");
}

#[test]
fn constructor_reference_is_typed_as_target_interface() {
    let src = r#"
interface Maker { String make(); }

class C {
    void m() {
        Maker m = String::new;
        m.make().length();
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.severity != nova_types::Severity::Error),
        "expected no errors; got {diags:?}"
    );
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "method-ref-mismatch"),
        "did not expect method-ref-mismatch diagnostic; got {diags:?}"
    );

    let offset = src
        .find("::")
        .expect("snippet should contain constructor reference");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "Maker");
}

#[test]
fn this_expression_types_as_enclosing_class() {
    let src = r#"
class C { C m(){ return this; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.severity != nova_types::Severity::Error),
        "expected no errors; got {diags:?}"
    );

    let offset = src.find("this").expect("snippet should contain this");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "C");
}

#[test]
fn super_expression_types_as_superclass() {
    let src = r#"
class A { int f(){ return 1; } }
class B extends A { int m(){ return super.f(); } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.severity != nova_types::Severity::Error),
        "expected no errors; got {diags:?}"
    );

    let offset = src.find("super").expect("snippet should contain super");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "A");
}

#[test]
fn array_index_must_be_integral() {
    let src = r#"
class C { void m(){ int[] a = {1}; int x = a[true]; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "invalid-array-index"),
        "expected invalid-array-index diagnostic; got {diags:?}"
    );
}

#[test]
fn array_access_on_non_array_is_error() {
    let src = r#"
class C { void m(){ int x = 0; int y = x[0]; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "invalid-array-access"),
        "expected invalid-array-access diagnostic; got {diags:?}"
    );
}

#[test]
fn array_creation_dimension_must_be_integral() {
    let src = r#"
class C { void m(){ int[] a = new int[true]; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "array-dimension-type"),
        "expected array-dimension-type diagnostic; got {diags:?}"
    );
}

#[test]
fn bit_not_promotes_byte_to_int() {
    let src = r#"
class C { int m(byte b){ return ~b; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.severity != nova_types::Severity::Error),
        "expected no errors; got {diags:?}"
    );

    let offset = src.find('~').expect("snippet should contain ~");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "int");
}

#[test]
fn pre_increment_preserves_byte_type() {
    let src = r#"
class C { void m(){ byte b = 0; byte c = ++b; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.severity != nova_types::Severity::Error),
        "expected no errors; got {diags:?}"
    );

    let offset = src.find("++").expect("snippet should contain ++");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "byte");
}

#[test]
fn instanceof_with_primitive_target_is_error() {
    let src = r#"
class C { void m(){ boolean b = "x" instanceof int; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "instanceof-invalid-type"),
        "expected instanceof-invalid-type diagnostic; got {diags:?}"
    );

    let offset = src.find("instanceof").expect("snippet should contain instanceof");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "boolean");
}

#[test]
fn instanceof_between_unrelated_types_is_error() {
    let src = r#"
class C { void m(){ boolean b = "x" instanceof Integer; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "invalid-instanceof"),
        "expected invalid-instanceof diagnostic; got {diags:?}"
    );

    let offset = src.find("instanceof").expect("snippet should contain instanceof");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "boolean");
}

#[test]
fn array_length_field_types_as_int() {
    let src = r#"
class C { int m(){ int[] a = {1,2}; return a.length; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.severity != nova_types::Severity::Error),
        "expected no errors; got {diags:?}"
    );

    let offset = src.find("length").expect("snippet should contain length");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "int");
}
