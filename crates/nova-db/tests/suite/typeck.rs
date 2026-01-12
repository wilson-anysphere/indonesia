use std::path::PathBuf;
use std::sync::Arc;

use nova_db::{ArcEq, FileId, NovaInputs, NovaTypeck, ProjectId, SalsaRootDatabase, SourceRootId};
use nova_jdk::JdkIndex;
use nova_project::{BuildSystem, JavaConfig, Module, ProjectConfig};
use tempfile::TempDir;

#[path = "../typeck/diagnostics.rs"]
mod diagnostics;

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
fn byte_initializer_allows_int_constant() {
    let src = r#"
class C { void m(){ byte b = 1; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "type-mismatch"),
        "expected no type-mismatch diagnostics; got {diags:?}"
    );
}

#[test]
fn byte_return_allows_int_constant() {
    let src = r#"
class C { byte m(){ return 1; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "return-mismatch"),
        "expected no return-mismatch diagnostics; got {diags:?}"
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
fn foreach_explicit_type_accepts_array_element() {
    let src = r#"
class C { void m(String[] arr){ for (String s : arr) { s.length(); } } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| {
            d.code.as_ref() != "type-mismatch"
                && d.code.as_ref() != "foreach-not-iterable"
                && d.code.as_ref() != "unresolved-method"
        }),
        "expected foreach over array to type-check without foreach/type mismatch errors, got {diags:?}"
    );
}

#[test]
fn foreach_explicit_type_rejects_incompatible_element() {
    let src = r#"
class C { void m(String[] arr){ for (int x : arr) {} } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "type-mismatch"),
        "expected foreach to report a type-mismatch diagnostic; got {diags:?}"
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
fn method_reference_is_typed_from_target() {
    let src = r#"
import java.util.function.Function;
class C {
    void m() {
        Function<String,Integer> f = String::length;
    }
}
"#;

    let (db, file) = setup_db(src);
    let offset = src
        .find("String::length")
        .expect("snippet should contain method reference")
        + "String::".len();
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "Function<String, Integer>");
}

#[test]
fn class_literal_is_typed_as_java_lang_class() {
    let src = r#"
class C { void m(){ Object x = String.class; } }
"#;

    let (db, file) = setup_db(src);
    let offset = src
        .find("String.class")
        .expect("snippet should contain class literal")
        + "String.".len();
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "Class<String>");
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
fn source_varargs_method_call_resolves() {
    let src = r#"
class C {
    static void foo(int... xs) {}
    void m() { foo(1, 2); }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected varargs method call to resolve, got {diags:?}"
    );
}

#[test]
fn target_typing_infers_generic_method_return_from_expected_type() {
    let src = r#"
import java.util.*;
class C {
    List<String> m() {
        return Collections.emptyList();
    }
}
"#;

    let (db, file) = setup_db(src);
    let offset = src
        .find("emptyList(")
        .expect("snippet should contain emptyList call")
        + "emptyList".len();
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "List<String>");
}

#[test]
fn unresolved_method_diagnostic_includes_candidates_and_arity() {
    let src = r#"
import java.util.*;
class C {
    void m() {
        Collections.emptyList(1);
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    let diag = diags
        .iter()
        .find(|d| d.code.as_ref() == "unresolved-method")
        .expect("expected unresolved-method diagnostic");

    assert!(
        diag.message.contains("emptyList"),
        "expected diagnostic message to mention method name, got {:?}",
        diag.message
    );
    assert!(
        diag.message.contains("wrong arity") && diag.message.contains("expected 0"),
        "expected diagnostic to mention arity/candidates, got {:?}",
        diag.message
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
fn cross_file_method_call_resolves_on_workspace_class() {
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

    let src_a = r#"package p; class A { String m(int x) { return ""; } }"#;
    let src_b = r#"package p; class B { String test() { return new A().m(1); } }"#;

    set_file(&mut db, project, a_file, "src/p/A.java", src_a);
    set_file(&mut db, project, b_file, "src/p/B.java", src_b);
    db.set_project_files(project, Arc::new(vec![a_file, b_file]));

    let offset = src_b.find("m(1)").expect("snippet should contain m call") + "m".len();
    let ty = db
        .type_at_offset_display(b_file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "String");

    let diags = db.type_diagnostics(b_file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected cross-file workspace method call to resolve, got {diags:?}"
    );
}

#[test]
fn cross_file_instance_method_call_resolves_on_workspace_class() {
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
        "package p; class A { void foo() {} }",
    );
    set_file(
        &mut db,
        project,
        b_file,
        "src/p/B.java",
        "package p; class B { void test() { new A().foo(); } }",
    );
    db.set_project_files(project, Arc::new(vec![a_file, b_file]));

    let diags = db.type_diagnostics(b_file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected cross-file workspace method call to resolve, got {diags:?}"
    );
}

#[test]
fn cross_file_signature_type_resolves_in_same_package() {
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

    set_file(&mut db, project, a_file, "src/p/Foo.java", "package p; class Foo {}");
    set_file(
        &mut db,
        project,
        b_file,
        "src/p/Bar.java",
        "package p; class Bar { void m() { Foo x; } }",
    );
    db.set_project_files(project, Arc::new(vec![a_file, b_file]));

    let diags = db.type_diagnostics(b_file);
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_ref() == "unresolved-type" && d.message.contains("Foo")),
        "expected `Foo` to resolve via workspace, got {diags:?}"
    );
}

#[test]
fn cross_file_generic_method_call_resolves_and_infers_return_type() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();
    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let gen_file = FileId::from_raw(1);
    let use_file = FileId::from_raw(2);

    set_file(
        &mut db,
        project,
        gen_file,
        "src/p/Gen.java",
        r#"package p; class Gen { <T> T id(T t) { return t; } }"#,
    );

    let use_src = r#"
package p;
class Use {
    String m() {
        Gen g = new Gen();
        return g.id("x");
    }
}
"#;
    set_file(&mut db, project, use_file, "src/p/Use.java", use_src);
    db.set_project_files(project, Arc::new(vec![gen_file, use_file]));

    let diags = db.type_diagnostics(use_file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected call to generic workspace method to resolve; got {diags:?}"
    );

    let offset = use_src
        .find("id(\"x\")")
        .expect("snippet should contain id call")
        + "id".len();
    let ty = db
        .type_at_offset_display(use_file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "String");
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

#[test]
fn expr_scopes_is_memoized_per_body() {
    let src = r#"
class C {
    String m() {
        String a = "x".substring(1);
        String b = "y".substring(2);
        return a + b;
    }
}
"#;

    let (db, file) = setup_db(src);
    db.clear_query_stats();

    let offset1 = src
        .find("substring(1)")
        .expect("snippet should contain first substring call")
        + "substring".len();
    let offset2 = src
        .rfind("substring(2)")
        .expect("snippet should contain second substring call")
        + "substring".len();

    let ty1 = db
        .type_at_offset_display(file, offset1 as u32)
        .expect("expected a type at first offset");
    let ty2 = db
        .type_at_offset_display(file, offset2 as u32)
        .expect("expected a type at second offset");
    assert_eq!(ty1, "String");
    assert_eq!(ty2, "String");

    let stats = db.query_stats();
    let expr_scopes_stat = stats
        .by_query
        .get("expr_scopes")
        .expect("expected expr_scopes query stat entry");
    assert_eq!(
        expr_scopes_stat.executions, 1,
        "expected expr_scopes to be memoized per body"
    );
}

#[test]
fn generic_method_type_params_do_not_trigger_unresolved_type() {
    let src = r#"
class C {
    <T> T id(T t) { return t; }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_ref() == "unresolved-type" && d.message.contains("`T`")),
        "expected `T` to resolve as a method type parameter, got {diags:?}"
    );
}

#[test]
fn generic_constructor_type_params_do_not_trigger_unresolved_type() {
    let src = r#"
class Foo {
    <T> Foo(T t) { }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_ref() == "unresolved-type" && d.message.contains("`T`")),
        "expected `T` to resolve as a constructor type parameter, got {diags:?}"
    );
}

#[test]
fn lambda_param_type_is_inferred_from_function_target() {
    let src = r#"
import java.util.function.Function;
class C {
    void m() {
        Function<String, Integer> f = s -> s.length();
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected lambda body method call to resolve after parameter inference, got {diags:?}"
    );

    let offset = src
        .find("s.length")
        .expect("snippet should contain lambda parameter usage");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "String");
}

#[test]
fn lambda_block_return_checked_against_sam() {
    let src = r#"
import java.util.function.Function;
class C {
    void m() {
        Function<String, Integer> f = s -> { return s.length(); };
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "return-mismatch"),
        "expected lambda return checking to use the SAM return type, got {diags:?}"
    );
}
