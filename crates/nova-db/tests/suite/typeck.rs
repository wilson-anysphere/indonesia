use std::path::PathBuf;
use std::sync::Arc;

use nova_classpath::{ClasspathEntry, ClasspathIndex};
use nova_db::salsa::FileExprId;
use nova_db::{
    ArcEq, FileId, NovaHir, NovaInputs, NovaTypeck, ProjectId, SalsaRootDatabase, SourceRootId,
};
use nova_hir::item_tree::{Item, Member};
use nova_jdk::JdkIndex;
use nova_project::{BuildSystem, JavaConfig, JavaVersion, Module, ProjectConfig};
use nova_resolve::ids::DefWithBodyId;
use nova_types::{PrimitiveType, Severity, Type, TypeEnv, TypeStore};
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

fn test_dep_jar() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../nova-classpath/testdata/dep.jar")
}

fn find_method_named(tree: &nova_hir::item_tree::ItemTree, name: &str) -> nova_hir::ids::MethodId {
    fn visit_item(
        tree: &nova_hir::item_tree::ItemTree,
        item: nova_hir::item_tree::Item,
        name: &str,
    ) -> Option<nova_hir::ids::MethodId> {
        use nova_hir::item_tree::{Item, Member};

        let members = match item {
            Item::Class(id) => &tree.class(id).members,
            Item::Interface(id) => &tree.interface(id).members,
            Item::Enum(id) => &tree.enum_(id).members,
            Item::Record(id) => &tree.record(id).members,
            Item::Annotation(id) => &tree.annotation(id).members,
        };

        for member in members {
            match member {
                Member::Method(id) if tree.method(*id).name == name => return Some(*id),
                Member::Type(child) => {
                    if let Some(found) = visit_item(tree, *child, name) {
                        return Some(found);
                    }
                }
                _ => {}
            }
        }
        None
    }

    for item in &tree.items {
        if let Some(id) = visit_item(tree, *item, name) {
            return id;
        }
    }

    panic!("method {name:?} not found in test fixture")
}

fn setup_db(text: &str) -> (SalsaRootDatabase, FileId) {
    setup_db_with_source(text, JavaVersion::JAVA_17)
}

fn setup_db_with_source(text: &str, source: JavaVersion) -> (SalsaRootDatabase, FileId) {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();
    let mut cfg = base_project_config(tmp.path().to_path_buf());
    cfg.java.source = source;
    cfg.java.target = source;
    db.set_project_config(project, Arc::new(cfg));
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let file = FileId::from_raw(1);
    set_file(&mut db, project, file, "src/Test.java", text);
    db.set_project_files(project, Arc::new(vec![file]));
    (db, file)
}

fn first_method_with_body(db: &SalsaRootDatabase, file: FileId) -> DefWithBodyId {
    let tree = db.hir_item_tree(file);
    for item in &tree.items {
        let members = match item {
            Item::Class(id) => &tree.class(*id).members,
            Item::Interface(id) => &tree.interface(*id).members,
            Item::Enum(id) => &tree.enum_(*id).members,
            Item::Record(id) => &tree.record(*id).members,
            Item::Annotation(id) => &tree.annotation(*id).members,
        };

        for member in members {
            if let Member::Method(m) = member {
                if tree.method(*m).body.is_some() {
                    return DefWithBodyId::Method(*m);
                }
            }
        }
    }

    panic!("no method with body found in file {:?}", file);
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
fn reports_type_mismatch_inside_labeled_statement() {
    let src = r#"
class C {
    void m() {
        label: { int x = "no"; }
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
fn cast_expression_changes_type() {
    let src = r#"
class C {
    void m() {
        Object o = "x";
        String s = (String) o;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "type-mismatch"),
        "expected cast expression to provide the target type; got {diags:?}"
    );
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "invalid-cast"),
        "expected valid cast to avoid invalid-cast diagnostic; got {diags:?}"
    );
}

#[test]
fn cast_expression_provides_target_type_for_var_and_lambda() {
    let src = r#"
class C {
    void m() {
        var r = (Runnable) () -> {};
        r.run();
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.code.as_ref() != "var-poly-expression"),
        "expected cast to provide a target type for lambda/var inference; got {diags:?}"
    );
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected inferred Runnable var to resolve run(); got {diags:?}"
    );
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "type-mismatch"),
        "expected no type mismatches; got {diags:?}"
    );
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "invalid-cast"),
        "expected no invalid-cast diagnostics; got {diags:?}"
    );
}

#[test]
fn cast_expression_provides_target_type_for_var_and_method_reference() {
    let src = r#"
import java.util.function.Function;

class C {
    void m() {
        var f = (Function<String, Integer>) String::length;
        f.apply("hi");
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.code.as_ref() != "var-poly-expression"),
        "expected cast to provide a target type for method reference/var inference; got {diags:?}"
    );
    assert!(
        diags
            .iter()
            .all(|d| d.code.as_ref() != "method-ref-without-target"),
        "expected cast to provide a target type for method references; got {diags:?}"
    );
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected inferred Function var to resolve apply(); got {diags:?}"
    );
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "invalid-cast"),
        "expected no invalid-cast diagnostics; got {diags:?}"
    );

    let offset = src
        .find("f.apply")
        .expect("snippet should contain f.apply");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "Function<String, Integer>");
}

#[test]
fn cast_expression_provides_target_type_for_var_and_constructor_reference() {
    let src = r#"
interface Maker { String make(); }

class C {
    void m() {
        var m = (Maker) String::new;
        m.make();
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "var-poly-expression"),
        "expected cast to provide a target type for constructor reference/var inference; got {diags:?}"
    );
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "method-ref-without-target"),
        "expected cast to provide a target type for constructor references; got {diags:?}"
    );
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected inferred Maker var to resolve make(); got {diags:?}"
    );
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "invalid-cast"),
        "expected no invalid-cast diagnostics; got {diags:?}"
    );
}

#[test]
fn cast_expression_provides_target_type_for_var_and_lambda_with_params() {
    let src = r#"
import java.util.function.Function;

class C {
    void m() {
        var f = (Function<String, Integer>) (s) -> s.length();
        f.apply("hi");
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "var-poly-expression"),
        "expected cast to provide a target type for lambda/var inference; got {diags:?}"
    );
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected inferred Function var to resolve apply()/length(); got {diags:?}"
    );
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "type-mismatch"),
        "expected no type mismatches; got {diags:?}"
    );
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "invalid-cast"),
        "expected no invalid-cast diagnostics; got {diags:?}"
    );

    let offset = src
        .find("f.apply")
        .expect("snippet should contain f.apply");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "Function<String, Integer>");
}

#[test]
fn explicit_type_args_affect_return_type() {
    let src = r#"
import java.util.List;
class C {
    void m() {
        List<String> xs = List.<String>of();
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected call to resolve, got {diags:?}"
    );
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "type-mismatch"),
        "expected no type-mismatch diagnostics, got {diags:?}"
    );
}

#[test]
fn invalid_explicit_type_args_emit_invalid_type_args_and_recover() {
    let src = r#"
import java.util.List;
class C {
    void m() {
        List<String> xs = List.<Missing>of();
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "invalid-type-args"),
        "expected invalid-type-args diagnostic, got {diags:?}"
    );
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected call to still resolve via inference/target typing, got {diags:?}"
    );
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "type-mismatch"),
        "expected no type-mismatch diagnostics, got {diags:?}"
    );
}

#[test]
fn explicit_type_args_influence_var_inference() {
    let src = r#"
import java.util.List;
class C {
    void m() {
        var xs = List.<String>of();
        xs.get(0).substring(1);
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| !(d.code.as_ref() == "unresolved-method" && d.message.contains("substring"))),
        "expected explicit type args to yield String element type, got {diags:?}"
    );

    let offset = src.find("get(0)").expect("snippet should contain get call") + "get".len();
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "String");
}

#[test]
fn explicit_generic_invocation_without_receiver_is_treated_as_unqualified_call() {
    let src = r#"
import java.util.*;
class C {
    static <T> List<T> emptyList() { return null; }
    void m() {
        <String>emptyList();
    }
}
"#;

    let (db, file) = setup_db(src);
    let offset = src
        .find("<String>emptyList(")
        .expect("snippet should contain emptyList call")
        + "<String>emptyList".len();
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "List<String>");
}

#[test]
fn constructor_type_args_do_not_become_method_type_args() {
    let src = r#"
class Foo {
    <T> Foo(T t) {}
    void bar() {}
    void m() {
        new <String> Foo("x").bar();
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected bar() call to resolve, got {diags:?}"
    );
}

#[test]
fn invalid_cast_produces_diagnostic() {
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
fn invalid_primitive_cast_produces_diagnostic() {
    let src = r#"
class C {
    void m() {
        int x = (int) "x";
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
fn unchecked_cast_emits_warning() {
    let src = r#"
class C {
    void m(Object o) {
        java.util.List<String> xs = (java.util.List<String>) o;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.severity == Severity::Warning && d.code.as_ref() == "unchecked"),
        "expected an unchecked warning diagnostic, got {diags:?}"
    );
}

#[test]
fn unchecked_raw_conversion_in_assignment_emits_warning() {
    let src = r#"
class C {
    void m(java.util.List raw) {
        java.util.List<String> xs = raw;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.severity == Severity::Warning && d.code.as_ref() == "unchecked"),
        "expected an unchecked warning diagnostic, got {diags:?}"
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
            .any(|d| d.code.as_ref() == "invalid-synchronized-expression"),
        "expected invalid-synchronized-expression diagnostic; got {diags:?}"
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
        "did not expect invalid-synchronized-expression diagnostic; got {diags:?}"
    );
}

#[test]
fn array_creation_has_array_type() {
    let src = r#"
class C { void m(){ int[] a = new int[1]; } }
"#;

    let (db, file) = setup_db(src);
    let offset = src
        .find("new int[1]")
        .expect("snippet should contain array creation")
        + "new ".len();
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "int[]");
}

#[test]
fn array_creation_multidimensional_has_array_type() {
    let src = r#"
class C { void m(){ int[][] a = new int[1][2]; } }
"#;

    let (db, file) = setup_db(src);
    let offset = src
        .find("new int[1][2]")
        .expect("snippet should contain array creation")
        + "new ".len();
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "int[][]");
}

#[test]
fn array_creation_dimension_type_must_be_integral() {
    let src = r#"
class C { void m(){ int[] a = new int[1.0]; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "array-dimension-type"),
        "expected array-dimension-type diagnostic; got {diags:?}"
    );
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
        "expected array initializer var decl to type-check; got {diags:?}"
    );
}

#[test]
fn array_initializer_element_type_mismatch_is_error() {
    let src = r#"
class C { void m(){ int[] a = {"x"}; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "array-initializer-type-mismatch"),
        "expected array-initializer-type-mismatch diagnostic; got {diags:?}"
    );
}

#[test]
fn array_creation_with_initializer_typechecks() {
    let src = r#"
class C { void m(){ int[] a = new int[] {1,2}; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| {
            d.code.as_ref() != "type-mismatch"
                && d.code.as_ref() != "invalid-array-initializer"
                && d.code.as_ref() != "array-initializer-type-mismatch"
        }),
        "expected array creation with initializer to type-check; got {diags:?}"
    );

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
fn rejects_non_statement_expression() {
    let src = r#"
class C {
    void m() {
        1 + 2;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "invalid-statement-expression"),
        "expected invalid-statement-expression diagnostic; got {diags:?}"
    );
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "invalid-expr-stmt"),
        "expected invalid-expr-stmt diagnostic; got {diags:?}"
    );
}

#[test]
fn allows_method_invocation_statement_expression() {
    let src = r#"
class C {
    void f() {}
    void m() {
        f();
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.code.as_ref() != "invalid-statement-expression"),
        "expected no invalid-statement-expression diagnostics; got {diags:?}"
    );
}

#[test]
fn rejects_parenthesized_method_invocation_statement_expression() {
    let src = r#"
class C {
    void f() {}
    void m() {
        (f());
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "invalid-statement-expression"),
        "expected invalid-statement-expression diagnostic; got {diags:?}"
    );
}

#[test]
fn allows_class_instance_creation_statement_expression() {
    let src = r#"
class C {
    void m() {
        new C();
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.code.as_ref() != "invalid-statement-expression"),
        "expected no invalid-statement-expression diagnostics; got {diags:?}"
    );
}

#[test]
fn rejects_array_creation_statement_expression() {
    let src = r#"
class C {
    void m() {
        new int[0];
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "invalid-statement-expression"),
        "expected invalid-statement-expression diagnostic; got {diags:?}"
    );
}

#[test]
fn rejects_parenthesized_assignment_statement_expression() {
    let src = r#"
class C {
    void m() {
        int i = 0;
        (i = 1);
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "invalid-statement-expression"),
        "expected invalid-statement-expression diagnostic; got {diags:?}"
    );
}

#[test]
fn rejects_parenthesized_assignment_statement_expression_with_comments() {
    let src = r#"
class C {
    void m() {
        int i = 0;
        (/*a*/ i = 1 /*b*/);
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "invalid-statement-expression"),
        "expected invalid-statement-expression diagnostic; got {diags:?}"
    );
}

#[test]
fn allows_parenthesized_receiver_method_invocation_statement_expression() {
    let src = r#"
class C {
    void f() {}
    void m() {
        (new C()).f();
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.code.as_ref() != "invalid-statement-expression"),
        "expected no invalid-statement-expression diagnostics; got {diags:?}"
    );
}

#[test]
fn allows_explicit_generic_invocation_statement_expression() {
    let src = r#"
class C {
    <T> void id(T t) {}
    void m() {
        this.<String>id("x");
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.code.as_ref() != "invalid-statement-expression"),
        "expected no invalid-statement-expression diagnostics; got {diags:?}"
    );
}

#[test]
fn allows_inc_dec_statement_expression() {
    let src = r#"
class C {
    void m() {
        int x = 0;
        x++;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.code.as_ref() != "invalid-statement-expression"),
        "expected no invalid-statement-expression diagnostics; got {diags:?}"
    );
}

#[test]
fn missing_expression_statement_does_not_emit_invalid_statement_expression() {
    let src = r#"
class C {
    void m() {
        ();
    }
}
"#;

    let (db, file) = setup_db(src);
    let owner = first_method_with_body(&db, file);
    let body = match owner {
        DefWithBodyId::Method(m) => db.hir_body(m),
        DefWithBodyId::Constructor(c) => db.hir_constructor_body(c),
        DefWithBodyId::Initializer(i) => db.hir_initializer_body(i),
    };

    let expr = body
        .stmts
        .iter()
        .find_map(|(_, stmt)| match stmt {
            nova_hir::hir::Stmt::Expr { expr, .. } => Some(*expr),
            _ => None,
        })
        .expect("expected an expression statement");

    assert!(
        matches!(&body.exprs[expr], nova_hir::hir::Expr::Missing { .. }),
        "expected expression statement to lower to Missing, got {:?}",
        body.exprs[expr]
    );

    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.code.as_ref() != "invalid-statement-expression"),
        "expected no invalid-statement-expression diagnostics; got {diags:?}"
    );
}

#[test]
fn rejects_non_statement_expression_in_for_update() {
    let src = r#"
class C {
    void m() {
        for (;; 1 + 2) {}
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "invalid-for-update-expression"),
        "expected invalid-for-update-expression diagnostic; got {diags:?}"
    );
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "invalid-for-update-expr"),
        "expected invalid-for-update-expr diagnostic; got {diags:?}"
    );
}

#[test]
fn allows_method_call_in_for_update() {
    let src = r#"
class C {
    void tick() {}
    void m() {
        for (;; tick()) {}
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.code.as_ref() != "invalid-for-update-expression"),
        "expected no invalid-for-update-expression diagnostics; got {diags:?}"
    );
}

#[test]
fn return_in_instance_initializer_is_error() {
    let src = r#"
class C {
  { return; }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "return-in-initializer"),
        "expected return-in-initializer diagnostic; got {diags:?}"
    );
}

#[test]
fn return_in_static_initializer_is_error() {
    let src = r#"
class C {
  static { return; }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "return-in-initializer"),
        "expected return-in-initializer diagnostic; got {diags:?}"
    );
}

#[test]
fn this_in_static_initializer_is_error() {
    let src = r#"
class C {
  static {
    this.toString();
  }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "this-in-static-context"
            && d.message.contains("static context")),
        "expected `this` in static initializer to produce a static-context diagnostic; got {diags:?}"
    );
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected no unresolved-method diagnostics when `this` is used in a static initializer; got {diags:?}"
    );
}

#[test]
fn super_in_static_initializer_is_error() {
    let src = r#"
class C {
  static {
    super.toString();
  }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "super-in-static-context"
            && d.message.contains("static context")),
        "expected `super` in static initializer to produce a static-context diagnostic; got {diags:?}"
    );
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected no unresolved-method diagnostics when `super` is used in a static initializer; got {diags:?}"
    );
}

#[test]
fn this_and_super_in_instance_initializer_are_allowed() {
    let src = r#"
class C {
  {
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
            .all(|d| d.code.as_ref() != "this-in-static-context"
                && d.code.as_ref() != "super-in-static-context"),
        "expected instance initializer to allow `this`/`super`; got {diags:?}"
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
fn byte_assignment_allows_int_constant() {
    let src = r#"
class C { void m(){ byte b; b = 1; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "type-mismatch"),
        "expected no type-mismatch diagnostics; got {diags:?}"
    );
}

#[test]
fn byte_initializer_allows_negative_int_constant() {
    let src = r#"
class C { void m(){ byte b = -1; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "type-mismatch"),
        "expected no type-mismatch diagnostics; got {diags:?}"
    );
}

#[test]
fn byte_initializer_allows_hex_int_constant() {
    let src = r#"
class C { void m(){ byte b = 0x7f; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "type-mismatch"),
        "expected no type-mismatch diagnostics; got {diags:?}"
    );
}

#[test]
fn byte_initializer_allows_hex_twos_complement_int_constant() {
    let src = r#"
class C { void m(){ byte b = 0xffffffff; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "type-mismatch"),
        "expected no type-mismatch diagnostics; got {diags:?}"
    );
}

#[test]
fn byte_initializer_allows_constant_expression() {
    let src = r#"
class C { void m(){ byte b = 1 + 2; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "type-mismatch"),
        "expected no type-mismatch diagnostics; got {diags:?}"
    );
}

#[test]
fn byte_initializer_allows_char_constant() {
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
fn byte_initializer_rejects_out_of_range_char_constant() {
    let src = r#"
class C { void m(){ byte b = '\u0100'; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "type-mismatch"),
        "expected `byte b = '\\u0100'` to produce a type-mismatch diagnostic; got {diags:?}"
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
fn assignment_to_literal_is_error() {
    let src = r#"
class C {
    void m() {
        1 = 2;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "invalid-assignment-target"),
        "expected invalid-assignment-target diagnostic, got {diags:?}"
    );
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "invalid-lvalue"),
        "expected invalid-lvalue diagnostic, got {diags:?}"
    );
}

#[test]
fn increment_literal_is_error() {
    let src = r#"
class C {
    void m() {
        ++1;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "invalid-lvalue"),
        "expected invalid-lvalue diagnostic, got {diags:?}"
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
fn foreach_explicit_type_accepts_list_element() {
    let src = r#"
import java.util.*;
class C { void m(List<String> list){ for (String s : list) { s.length(); } } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| {
            d.code.as_ref() != "type-mismatch"
                && d.code.as_ref() != "foreach-not-iterable"
                && d.code.as_ref() != "unresolved-method"
        }),
        "expected foreach over list to type-check without foreach/type mismatch errors, got {diags:?}"
    );
}

#[test]
fn foreach_var_infers_list_element() {
    let src = r#"
import java.util.*;
class C { void m(List<String> list){ for (var s : list) { s.length(); } } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| {
            d.code.as_ref() != "foreach-not-iterable"
                && d.code.as_ref() != "cannot-infer-foreach-var"
                && d.code.as_ref() != "type-mismatch"
                && d.code.as_ref() != "unresolved-method"
        }),
        "expected foreach var over list to infer element type, got {diags:?}"
    );
}

#[test]
fn compound_assign_allows_narrowing_like_javac() {
    let src = r#"
class C {
    void m() {
        byte b = 0;
        b += 1;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "type-mismatch"),
        "expected no type-mismatch diagnostics; got {diags:?}"
    );
}

#[test]
fn compound_assign_add_string_concat_rhs_is_allowed() {
    let src = r#"
class C {
    void m() {
        Object o = "a";
        o += "b";
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "type-mismatch"),
        "expected `o += \"b\"` to type-check via string concatenation; got {diags:?}"
    );
}

#[test]
fn compound_assign_rejects_non_numeric() {
    let src = r#"
class C {
    void m() {
        boolean b = true;
        b += 1;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "type-mismatch"),
        "expected a diagnostic for invalid compound assignment; got {diags:?}"
    );
}

#[test]
fn inc_requires_numeric() {
    let src = r#"
class C {
    void m() {
        boolean b = true;
        b++;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "invalid-inc-dec"),
        "expected invalid-inc-dec diagnostic; got {diags:?}"
    );
}

#[test]
fn post_inc_preserves_byte_type() {
    let src = r#"
class C {
    void m() {
        byte b = 0;
        byte c = b++;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "type-mismatch"),
        "expected `byte c = b++` to type-check (b++ is byte in Java), got {diags:?}"
    );
    assert!(
        !diags.iter().any(|d| d.code.as_ref() == "invalid-inc-dec"),
        "expected b++ to be accepted for byte locals, got {diags:?}"
    );

    let offset = src.find("++").expect("test source should contain ++");
    let ty = db
        .type_at_offset_display(file, (offset + 1) as u32)
        .expect("expected type at offset");
    assert_eq!(ty, "byte");
}

#[test]
fn post_inc_preserves_typevar_type() {
    let src = r#"
class C {
    <T extends java.lang.Integer> void m(T t) {
        T u = t++;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "type-mismatch"),
        "expected `T u = t++` to type-check, got {diags:?}"
    );
    assert!(
        !diags.iter().any(|d| d.code.as_ref() == "invalid-inc-dec"),
        "expected t++ to be accepted for type vars with unboxable numeric bounds, got {diags:?}"
    );

    let offset = src.find("++").expect("test source should contain ++");
    let ty = db
        .type_at_offset_display(file, (offset + 1) as u32)
        .expect("expected type at offset");
    assert_eq!(ty, "T");
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
fn assert_condition_must_be_boolean() {
    let src = r#"
class C { void m(){ assert 1; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "assert-condition-not-boolean"),
        "expected assert-condition-not-boolean diagnostic; got {diags:?}"
    );
}

#[test]
fn assert_with_message_typechecks_message_expr() {
    let src = r#"
class C { void m(){ String s = "x"; assert s.isEmpty() : s.length(); } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.code.as_ref() != "invalid-statement-expression"),
        "expected no invalid-statement-expression diagnostics; got {diags:?}"
    );
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected String methods in assert to resolve; got {diags:?}"
    );
}

#[test]
fn boxed_boolean_conditions_are_allowed() {
    let src = r#"
class C {
    void m(java.lang.Boolean b) {
        if (b) {}
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_ref() == "condition-not-boolean"),
        "expected boxed Boolean to be allowed in condition context, got {diags:?}"
    );
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_ref() == "unresolved-type" && d.message.contains("Boolean")),
        "expected java.lang.Boolean to resolve, got {diags:?}"
    );
}

#[test]
fn not_allows_boxed_boolean_operand() {
    let src = r#"
class C {
    boolean m(java.lang.Boolean b) { return !b; }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        !diags.iter().any(|d| d.code.as_ref() == "invalid-unary-op"),
        "expected `!Boolean` to type-check via unboxing, got {diags:?}"
    );
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_ref() == "unresolved-type" && d.message.contains("Boolean")),
        "expected java.lang.Boolean to resolve, got {diags:?}"
    );
}

#[test]
fn reports_condition_not_boolean_for_ternary() {
    let src = r#"
class C {
    int m() {
        return 1 ? 2 : 3;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "condition-not-boolean"),
        "expected condition-not-boolean diagnostic for ternary condition, got {diags:?}"
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
fn type_at_offset_display_is_demand_driven_and_does_not_execute_typeck_body() {
    let src = r#"
class C {
    String m() {
        return "x".substring(1);
    }

    // Large, unrelated body with a type error. Hover should still be demand-driven and avoid
    // forcing full-body type checking of `heavy`.
    void heavy() {
        int y = "no";
        int x0 = 0;
        int x1 = x0 + 1;
        int x2 = x1 + 1;
        int x3 = x2 + 1;
        int x4 = x3 + 1;
        int x5 = x4 + 1;
        int x6 = x5 + 1;
        int x7 = x6 + 1;
        int x8 = x7 + 1;
        int x9 = x8 + 1;
    }
}
"#;

    let (db, file) = setup_db(src);
    let offset = src
        .find("substring(")
        .expect("snippet should contain substring call")
        + "substring".len();

    // Reset query stats so the assertion below only reflects the `type_at_offset_display` call.
    db.clear_query_stats();

    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "String");

    let typeck_body_executions = db
        .query_stats()
        .by_query
        .get("typeck_body")
        .map(|s| s.executions)
        .unwrap_or(0);
    assert_eq!(
        typeck_body_executions, 0,
        "type_at_offset_display should not execute typeck_body"
    );
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
class C { double m(){ return 1.0d; } }
"#;

    let (db, file) = setup_db(src);
    let offset = src
        .find("1.0d")
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
fn array_access_returns_element_type() {
    let src = r#"
class C { int m(int[] a){ return a[0]; } }
"#;

    let (db, file) = setup_db(src);
    let offset = src.find("a[0]").expect("snippet should contain a[0]") + 1; // at `[`
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "int");
}

#[test]
fn indexing_non_array_is_error() {
    let src = r#"
class C { int m(int a){ return a[0]; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "invalid-array-access"),
        "expected invalid-array-access diagnostic; got {diags:?}"
    );
}

#[test]
fn indexing_non_array_does_not_also_report_invalid_index() {
    let src = r#"
class C { int m(int a, boolean b){ return a[b]; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "invalid-array-access"),
        "expected invalid-array-access diagnostic; got {diags:?}"
    );
    assert!(
        !diags.iter().any(|d| d.code.as_ref() == "invalid-array-index"),
        "did not expect invalid-array-index when receiver is not an array; got {diags:?}"
    );
}

#[test]
fn indexing_with_non_integral_index_is_error() {
    let src = r#"
class C { int m(int[] a, boolean b){ return a[b]; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "invalid-array-index"),
        "expected invalid-array-index diagnostic; got {diags:?}"
    );
}

#[test]
fn array_access_allows_boxed_integer_index() {
    let src = r#"
class C { int m(int[] a, Integer i){ return a[i]; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_ref() == "invalid-array-index"),
        "expected no invalid-array-index diagnostic; got {diags:?}"
    );

    let offset = src.find("a[i]").expect("snippet should contain a[i]") + 1; // at `[`
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "int");
}

#[test]
fn assignment_to_array_access_is_allowed() {
    let src = r#"
class C { void m(int[] a){ a[0] = 1; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.code.as_ref() != "invalid-assignment-target"),
        "expected no invalid-assignment-target diagnostic; got {diags:?}"
    );
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "type-mismatch"),
        "expected no type-mismatch diagnostic; got {diags:?}"
    );
}

#[test]
fn assignment_to_array_access_checks_element_type() {
    let src = r#"
class C { void m(int[] a){ a[0] = "no"; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "type-mismatch"),
        "expected type-mismatch diagnostic; got {diags:?}"
    );
    assert!(
        diags
            .iter()
            .all(|d| d.code.as_ref() != "invalid-assignment-target"),
        "expected no invalid-assignment-target diagnostic; got {diags:?}"
    );
}

#[test]
fn array_creation_allows_boxed_integer_dimension() {
    let src = r#"
class C { int[] m(Integer i){ return new int[i]; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_ref() == "array-dimension-type"),
        "expected no array-dimension-type diagnostic; got {diags:?}"
    );
}

#[test]
fn qualified_type_receiver_resolves_for_static_call() {
    let src = r#"
class C {
    String m() { return java.lang.String.valueOf(1); }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.code.as_ref() != "unresolved-name" && d.code.as_ref() != "unresolved-field"),
        "expected no unresolved-name/unresolved-field diagnostics, got {diags:?}"
    );

    let offset = src
        .find("valueOf(")
        .expect("snippet should contain valueOf call")
        + "valueOf".len();
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "String");
}

#[test]
fn conditional_with_null_infers_reference_type() {
    let src = r#"
class C {
    void m(boolean cond) {
        var s = cond ? "a" : null;
        s.substring(1);
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.code.as_ref() != "unresolved-method" || !d.message.contains("substring")),
        "expected substring call to resolve, got {diags:?}"
    );

    let offset = src.find('?').expect("snippet should contain ?");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "String");
}

#[test]
fn conditional_infers_common_superclass_via_lub() {
    let src = r#"
class Base {
    void base() {}
}
class A extends Base {}
class B extends Base {}
class C {
    void m(boolean cond) {
        var o = cond ? new A() : new B();
        o.base();
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.code.as_ref() != "unresolved-method" || !d.message.contains("base")),
        "expected Base.base() call to resolve, got {diags:?}"
    );

    let offset = src.find('?').expect("snippet should contain ?");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "Base");
}

#[test]
fn conditional_unboxes_boxed_numeric_types() {
    let src = r#"
class C {
    void m(boolean cond, Integer a, Long b) {
        var x = cond ? a : b;
        long y = x;
    }
}
"#;

    let (db, file) = setup_db(src);
    let offset = src.find('?').expect("snippet should contain ?");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "long");
}

#[test]
fn plus_unboxes_integer_operands_to_int() {
    let src = r#"
class C {
    int m(Integer a, Integer b) {
        return a + b;
    }
}
"#;

    let (db, file) = setup_db(src);
    let offset = src.find('+').expect("snippet should contain +");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "int");
}

#[test]
fn type_at_offset_shows_boolean_for_relational_comparison() {
    let src = r#"
class C {
    boolean m() {
        return 1 < 2;
    }
}
"#;

    let (db, file) = setup_db(src);
    let offset = src.find('<').expect("snippet should contain <");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "boolean");
}

#[test]
fn type_at_offset_shows_boolean_for_equality_comparison() {
    let src = r#"
class C {
    boolean m() {
        return 1 == 2;
    }
}
"#;

    let (db, file) = setup_db(src);
    let offset = src.find("==").expect("snippet should contain ==");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "boolean");
}

#[test]
fn instanceof_has_boolean_type() {
    let src = r#"
class C {
    boolean m(Object o){
        return o instanceof String;
    }
}
"#;

    let (db, file) = setup_db(src);
    let offset = src
        .find("String")
        .expect("snippet should contain instanceof type");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "boolean");
}

#[test]
fn instanceof_rhs_primitive_is_error() {
    let src = r#"
class C {
    boolean m(Object o){
        return o instanceof int;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "instanceof-invalid-type"),
        "expected instanceof-invalid-type diagnostic; got {diags:?}"
    );
}

#[test]
fn instanceof_rhs_void_is_error() {
    let src = r#"
class C {
    boolean m(Object o){
        return o instanceof void;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "instanceof-void"),
        "expected instanceof-void diagnostic; got {diags:?}"
    );
}

#[test]
fn instanceof_lhs_primitive_is_error() {
    let src = r#"
class C {
    boolean m(int i){
        return i instanceof String;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "instanceof-primitive"),
        "expected instanceof-primitive diagnostic; got {diags:?}"
    );
}

#[test]
fn instanceof_incompatible_reference_types_is_error() {
    let src = r#"
class C {
    boolean m(String s){
        return s instanceof Integer;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "invalid-instanceof"),
        "expected invalid-instanceof diagnostic; got {diags:?}"
    );
}

#[test]
fn boxed_primitive_equality_with_null_is_boolean() {
    let src = r#"
class C {
    boolean m(Integer i) {
        return i == null;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "type-mismatch"),
        "expected no type-mismatch diagnostics; got {diags:?}"
    );

    let offset = src.find("==").expect("snippet should contain ==");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "boolean");
}

#[test]
fn boxed_primitive_equality_between_incomparable_wrappers_is_error() {
    let src = r#"
class C {
    boolean m(Integer a, Long b) {
        return a == b;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "type-mismatch"),
        "expected type-mismatch diagnostic; got {diags:?}"
    );
}

#[test]
fn type_at_offset_shows_boolean_for_logical_and() {
    let src = r#"
class C {
    boolean m() {
        return true && false;
    }
}
"#;

    let (db, file) = setup_db(src);
    let offset = src.find("&&").expect("snippet should contain &&");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "boolean");
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
fn method_reference_initializer_does_not_report_type_mismatch() {
    let src = r#"
 import java.util.function.Function;
class C {
    void m() {
        Function<String,Integer> f = String::length;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "type-mismatch"),
        "expected no type-mismatch diagnostics; got {diags:?}"
    );
}

#[test]
fn method_reference_mismatch_reports_method_ref_diag() {
    let src = r#"
 import java.util.function.Function;
class C {
    void m() {
        Function<String,Integer> f = String::isEmpty;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "method-ref-mismatch"),
        "expected method-ref-mismatch diagnostic; got {diags:?}"
    );
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "type-mismatch"),
        "expected no type-mismatch diagnostics; got {diags:?}"
    );
}

#[test]
fn method_reference_is_typed_from_call_argument_target() {
    let src = r#"
 import java.util.function.Function;
class C {
    static void take(Function<String, Integer> f) {}
    void m() {
        C.take(String::length);
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
fn constructor_reference_is_typed_from_target() {
    let src = r#"
interface Maker { String make(); }
class C {
    void m() {
        Maker x = String::new;
    }
}
"#;

    let (db, file) = setup_db(src);
    let offset = src
        .find("String::new")
        .expect("snippet should contain constructor reference")
        + "String::".len();
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "Maker");
}

#[test]
fn constructor_reference_mismatch_reports_method_ref_diag() {
    let src = r#"
 import java.util.function.Supplier;
class C {
    void m() {
        Supplier<Integer> s = String::new;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "method-ref-mismatch"),
        "expected method-ref-mismatch diagnostic; got {diags:?}"
    );
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "type-mismatch"),
        "expected no type-mismatch diagnostics; got {diags:?}"
    );
}

#[test]
fn constructor_reference_is_typed_from_call_argument_target() {
    let src = r#"
interface Maker { String make(); }
class C {
    static void take(Maker m) {}
    void m() {
        C.take(String::new);
    }
}
"#;

    let (db, file) = setup_db(src);
    let offset = src
        .find("String::new")
        .expect("snippet should contain constructor reference")
        + "String::".len();
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "Maker");
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
    assert!(
        ty.contains("Class") && ty.contains("String"),
        "expected class literal type to mention Class<String>, got {ty:?}"
    );
}

#[test]
fn primitive_class_literal_is_typed_as_java_lang_class_wildcard() {
    let src = r#"
class C { void m(){ Object x = int.class; } }
"#;

    let (db, file) = setup_db(src);
    let offset = src
        .find("int.class")
        .expect("snippet should contain class literal")
        + "int.".len();
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert!(
        ty.contains("Class") && ty.contains('?'),
        "expected primitive class literal type to be Class<?>, got {ty:?}"
    );
}

#[test]
fn class_literal_infers_var_type_as_java_lang_class() {
    let src = r#"
class C {
    void m() {
        var c = String.class;
        c.toString();
    }
}
"#;

    let (db, file) = setup_db(src);
    let offset = src
        .find("c.toString")
        .expect("snippet should contain c.toString");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "Class<String>");
}

#[test]
fn type_at_offset_finds_explicit_constructor_invocation() {
    let src = r#"
class C {
    C() { this(1); }
    C(int x) {}
}
"#;

    let (db, file) = setup_db(src);
    // Place the cursor on the literal argument to ensure the invocation was
    // lowered all the way into HIR/typeck (not dropped or replaced with a
    // missing expression).
    let offset = src
        .find("this(1)")
        .expect("snippet should contain explicit constructor invocation")
        + "this(".len();
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset inside explicit constructor invocation");
    assert_eq!(ty, "int");
}

#[test]
fn method_reference_is_typed_from_explicit_constructor_invocation_target() {
    let src = r#"
import java.util.function.Function;
class C {
    C(Function<String, Integer> f) {}
    C() { this(String::length); }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.code.as_ref() != "method-ref-without-target"),
        "expected ctor invocation to target-type method reference; got {diags:?}"
    );
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-constructor"),
        "expected constructor invocation to resolve; got {diags:?}"
    );

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
fn lambda_argument_in_explicit_constructor_invocation_is_target_typed() {
    let src = r#"
class C {
    C(Runnable r) {}
    C() { this(() -> 1); }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "return-mismatch"),
        "expected return-mismatch diagnostic for void-compatible lambda; got {diags:?}"
    );
}

#[test]
fn resolves_explicit_this_constructor_invocation() {
    let src = r#"
class C {
    C() { this(1); }
    C(int x) {}
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| {
            d.code.as_ref() != "unresolved-constructor"
                && d.code.as_ref() != "ambiguous-constructor"
                && d.code.as_ref() != "invalid-constructor-invocation"
                && d.code.as_ref() != "constructor-invocation-not-first"
        }),
        "expected `this(1)` to resolve without ctor diagnostics; got {diags:?}"
    );
}

#[test]
fn super_invocation_outside_constructor_is_error() {
    let src = r#"
class C {
    void m() {
        super();
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "invalid-constructor-invocation"),
        "expected invalid-constructor-invocation diagnostic, got {diags:?}"
    );
}

#[test]
fn super_invocation_not_first_is_error() {
    let src = r#"
class C {
    C() {
        int x = 0;
        super();
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "constructor-invocation-not-first"),
        "expected constructor-invocation-not-first diagnostic, got {diags:?}"
    );
}

#[test]
fn this_invocation_not_first_is_error() {
    let src = r#"
class C {
    C() {
        int x = 0;
        this(1);
    }
    C(int x) {}
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "constructor-invocation-not-first"),
        "expected constructor-invocation-not-first diagnostic, got {diags:?}"
    );

    let bad = diags
        .iter()
        .find(|d| d.code.as_ref() == "constructor-invocation-not-first")
        .expect("expected constructor-invocation-not-first diagnostic");
    let span = bad
        .span
        .expect("constructor-invocation-not-first diagnostic should have a span");
    let this_kw = src
        .find("this(1)")
        .expect("snippet should contain this call");
    assert!(
        span.start <= this_kw && this_kw < span.end,
        "expected diagnostic span to cover this invocation, got {span:?}"
    );
}

#[test]
fn this_invocation_in_nested_block_is_error() {
    let src = r#"
class C {
    C() {
        { this(1); }
    }
    C(int x) {}
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "constructor-invocation-not-first"),
        "expected constructor-invocation-not-first diagnostic, got {diags:?}"
    );

    let bad = diags
        .iter()
        .find(|d| d.code.as_ref() == "constructor-invocation-not-first")
        .expect("expected constructor-invocation-not-first diagnostic");
    let span = bad
        .span
        .expect("constructor-invocation-not-first diagnostic should have a span");
    let this_kw = src.find("this(1)").expect("snippet should contain this call");
    assert!(
        span.start <= this_kw && this_kw < span.end,
        "expected diagnostic span to cover this invocation, got {span:?}"
    );
}

#[test]
fn super_invocation_in_lambda_is_error() {
    let src = r#"
class C {
    C() {
        Runnable r = () -> { super(); };
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "constructor-invocation-not-first"),
        "expected constructor-invocation-not-first diagnostic, got {diags:?}"
    );

    let bad = diags
        .iter()
        .find(|d| d.code.as_ref() == "constructor-invocation-not-first")
        .expect("expected constructor-invocation-not-first diagnostic");
    let span = bad
        .span
        .expect("constructor-invocation-not-first diagnostic should have a span");
    let super_kw = src.find("super()").expect("snippet should contain super() call");
    assert!(
        span.start <= super_kw && super_kw < span.end,
        "expected diagnostic span to cover super() invocation, got {span:?}"
    );
}

#[test]
fn explicit_super_invocation_resolves_object_ctor() {
    let src = r#"
class C {
    C() {
        super();
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.code.as_ref() != "unresolved-constructor"),
        "expected super() to resolve against Object(), got {diags:?}"
    );
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
        diags
            .iter()
            .any(|d| d.code.as_ref() == "static-context" && d.message.contains("static context")),
        "expected static context to reject implicit-this call, got {diags:?}"
    );
}

#[test]
fn this_in_static_method_emits_static_context_diagnostic() {
    let src = r#"
class C {
  static Object m() {
    return this;
  }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "this-in-static-context"),
        "expected `this` in a static method to produce a static-context diagnostic; got {diags:?}"
    );
}

#[test]
fn super_in_static_method_emits_static_context_diagnostic() {
    let src = r#"
class C {
  static Object m() {
    return super;
  }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "super-in-static-context"),
        "expected `super` in a static method to produce a static-context diagnostic; got {diags:?}"
    );
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
        diags
            .iter()
            .any(|d| d.code.as_ref() == "static-context" && d.message.contains("static context")),
        "expected static context to reject implicit-this field access, got {diags:?}"
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
        "expected implicit-this access to a static field to be allowed, got {diags:?}"
    );
}

#[test]
fn static_context_rejects_unqualified_instance_field_access_in_static_initializer() {
    let src = r#"
class C {
    int x;
    static {
        x = 1;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "static-context"
            && d.message.contains("static context")),
        "expected static context to reject implicit-this field access in static initializer, got {diags:?}"
    );
}

#[test]
fn static_context_allows_instance_field_access_via_explicit_receiver() {
    let src = r#"
class C {
    int x;
    static int m() {
        C c = new C();
        c.x = 1;
        return c.x;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "static-context"),
        "expected explicit receiver field access to be allowed in a static method; got {diags:?}"
    );
}

#[test]
fn static_context_rejects_unqualified_instance_field_access_in_lambda() {
    let src = r#"
class C {
    int x;
    static void m() {
        Runnable r = () -> { x = 1; };
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "static-context" && d.message.contains("static context")),
        "expected static context to reject implicit-this field access inside lambda, got {diags:?}"
    );
}

#[test]
fn fully_qualified_static_method_call_resolves() {
    let src = r#"
class C {
    void m() {
        java.lang.String.valueOf(1);
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    let unresolved_members = diags
        .iter()
        .filter(|d| d.code.as_ref() == "unresolved-field" || d.code.as_ref() == "unresolved-method")
        .collect::<Vec<_>>();
    assert!(
        unresolved_members.is_empty(),
        "expected no unresolved member diagnostics, got {unresolved_members:?} (all diags: {diags:?})"
    );
}

#[test]
fn static_context_allows_unqualified_enum_constant_access() {
    let src = r#"
enum E {
    A;
    static E m() {
        return A;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.code.as_ref() != "static-context" && d.code.as_ref() != "unresolved-name"),
        "expected enum constant to resolve as an implicit static field, got {diags:?}"
    );
}

#[test]
fn static_context_allows_unqualified_interface_field_access() {
    let src = r#"
interface I {
    int X = 1;
    static int m() {
        return X;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.code.as_ref() != "static-context" && d.code.as_ref() != "unresolved-name"),
        "expected interface field to resolve as an implicit static field, got {diags:?}"
    );
}

#[test]
fn system_out_println_has_no_unresolved_member_diags() {
    let src = r#"
class C {
    void m() {
        System.out.println("x");
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    let unresolved_members = diags
        .iter()
        .filter(|d| d.code.as_ref() == "unresolved-field" || d.code.as_ref() == "unresolved-method")
        .collect::<Vec<_>>();
    assert!(
        unresolved_members.is_empty(),
        "expected no unresolved member diagnostics, got {unresolved_members:?} (all diags: {diags:?})"
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
fn object_methods_are_available_via_superclass() {
    let src = r#"
class C {
    void m(C other) {
        this.toString();
        this.equals(other);
        this.hashCode();
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);

    for name in ["toString", "equals", "hashCode"] {
        assert!(
            diags
                .iter()
                .all(|d| { d.code.as_ref() != "unresolved-method" || !d.message.contains(name) }),
            "expected `{name}` to resolve via java.lang.Object, got {diags:?}"
        );
    }
}

#[test]
fn java_lang_enum_resolves_and_has_ordinal_method() {
    let src = r#"
class C {
    void m(java.lang.Enum e) {
        e.ordinal();
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-type"),
        "expected java.lang.Enum to resolve; got {diags:?}"
    );
    assert!(
        diags
            .iter()
            .all(|d| d.code.as_ref() != "unresolved-method" || !d.message.contains("ordinal")),
        "expected java.lang.Enum.ordinal() to resolve; got {diags:?}"
    );
}

#[test]
fn java_lang_record_resolves_and_inherits_object_methods() {
    let src = r#"
class C {
    void m(java.lang.Record r) {
        r.toString();
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-type"),
        "expected java.lang.Record to resolve; got {diags:?}"
    );
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected java.lang.Record to inherit java.lang.Object methods; got {diags:?}"
    );
}

#[test]
fn java_lang_annotation_annotation_resolves() {
    let src = r#"
class C {
    void m(java.lang.annotation.Annotation a) {
        a.toString();
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-type"),
        "expected java.lang.annotation.Annotation to resolve; got {diags:?}"
    );
}

#[test]
fn object_constructor_is_available() {
    let src = r#"
class C {
    void m() {
        new Object();
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.code.as_ref() != "unresolved-constructor"),
        "expected `new Object()` to resolve its implicit no-arg constructor; got {diags:?}"
    );
}

#[test]
fn object_get_class_method_is_available() {
    let src = r#"
class C {
    void m() {
        Class<?> c = this.getClass();
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| { d.code.as_ref() != "unresolved-method" || !d.message.contains("getClass") }),
        "expected `getClass` to resolve via java.lang.Object, got {diags:?}"
    );
}

#[test]
fn static_method_called_via_instance_emits_warning() {
    let src = r#"
class C {
    static int foo(){ return 1; }
    int m(){ C c = new C(); return c.foo(); }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    let count = diags
        .iter()
        .filter(|d| {
            d.severity == Severity::Warning && d.code.as_ref() == "static-access-via-instance"
        })
        .count();

    assert_eq!(
        count, 1,
        "expected one static-access-via-instance warning, got {diags:?}"
    );
}

#[test]
fn unqualified_static_method_call_does_not_emit_warning() {
    let src = r#"
class C {
    static void foo() {}
    void m(){ foo(); }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.code.as_ref() != "static-access-via-instance"),
        "expected no static-access-via-instance warning for unqualified static call, got {diags:?}"
    );
}

#[test]
fn static_type_receiver_calling_instance_method_emits_static_context_diag() {
    let src = r#"
class C {
    void foo() {}
    static void m() {
        C.foo();
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    let static_diags: Vec<_> = diags
        .iter()
        .filter(|d| d.code.as_ref() == "static-context")
        .collect();
    assert_eq!(
        static_diags.len(),
        1,
        "expected exactly one static-context diagnostic, got {diags:?}"
    );
    let diag = static_diags[0];
    assert_eq!(
        diag.message, "cannot call instance method `foo` from a static context",
        "unexpected diagnostic message: {diag:?}"
    );
    let span = diag
        .span
        .expect("static-context diagnostic should have a span");
    let snippet = src
        .get(span.start..span.end)
        .unwrap_or("<invalid span>")
        .trim()
        .trim_end_matches(';');
    assert_eq!(snippet, "C.foo()", "unexpected span snippet for {diag:?}");

    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected no unresolved-method diagnostics; got {diags:?}"
    );
}

#[test]
fn static_type_receiver_calling_ambiguous_instance_method_emits_static_context_diag() {
    let src = r#"
class C {
    void foo(String s) {}
    void foo(Integer i) {}
    static void m() {
        C.foo(null);
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    let static_diags: Vec<_> = diags
        .iter()
        .filter(|d| d.code.as_ref() == "static-context")
        .collect();
    assert_eq!(
        static_diags.len(),
        1,
        "expected exactly one static-context diagnostic, got {diags:?}"
    );
    let diag = static_diags[0];
    assert_eq!(
        diag.message, "cannot call instance method `foo` from a static context",
        "unexpected diagnostic message: {diag:?}"
    );
    let span = diag
        .span
        .expect("static-context diagnostic should have a span");
    let snippet = src
        .get(span.start..span.end)
        .unwrap_or("<invalid span>")
        .trim()
        .trim_end_matches(';');
    assert_eq!(
        snippet, "C.foo(null)",
        "unexpected span snippet for {diag:?}"
    );

    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected no unresolved-method diagnostics; got {diags:?}"
    );
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "ambiguous-call"),
        "expected no ambiguous-call diagnostics; got {diags:?}"
    );
}

#[test]
fn static_type_receiver_accessing_instance_field_emits_static_context_diag() {
    let src = r#"
class C {
    int x;
    static int m() {
        return C.x;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    let static_diags: Vec<_> = diags
        .iter()
        .filter(|d| d.code.as_ref() == "static-context")
        .collect();
    assert_eq!(
        static_diags.len(),
        1,
        "expected exactly one static-context diagnostic, got {diags:?}"
    );
    let diag = static_diags[0];
    assert_eq!(
        diag.message, "cannot reference instance field `x` from a static context",
        "unexpected diagnostic message: {diag:?}"
    );
    let span = diag
        .span
        .expect("static-context diagnostic should have a span");
    let snippet = src
        .get(span.start..span.end)
        .unwrap_or("<invalid span>")
        .trim();
    assert_eq!(snippet, "C.x", "unexpected span snippet for {diag:?}");

    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-field"),
        "expected no unresolved-field diagnostics; got {diags:?}"
    );
}

#[test]
fn warns_on_static_field_access_via_instance() {
    let src = r#"
class C {
    static int X = 1;
    int m(){ C c = new C(); return c.X; }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| {
            d.severity == Severity::Warning && d.code.as_ref() == "static-access-via-instance"
        }),
        "expected a static-access-via-instance warning; got {diags:?}"
    );
}

#[test]
fn static_method_cannot_access_instance_field_unqualified() {
    let src = r#"
class C { int x; static int m() { return x; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "static-context"),
        "expected a static-context diagnostic, got {diags:?}"
    );
}

#[test]
fn enum_constants_are_static_fields() {
    let src = r#"
enum E {
    A;
    static E m() {
        return E.A;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.code.as_ref() != "static-context" && d.code.as_ref() != "unresolved-field"),
        "expected enum constant access to resolve as a static field; got {diags:?}"
    );
}

#[test]
fn interface_fields_are_static_fields() {
    let src = r#"
interface I { int X = 1; }
class C {
    static int m() {
        return I.X;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.code.as_ref() != "static-context" && d.code.as_ref() != "unresolved-field"),
        "expected interface field access to resolve as a static field; got {diags:?}"
    );
}

#[test]
fn enum_constants_are_static_fields_unqualified() {
    let src = r#"
enum E {
    A;
    static E m() {
        return A;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "static-context"),
        "expected unqualified enum constant reference in a static context to be allowed; got {diags:?}"
    );
}

#[test]
fn interface_fields_are_static_fields_unqualified() {
    let src = r#"
interface I {
    int X = 1;
    static int m() {
        return X;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "static-context"),
        "expected unqualified interface field reference in a static context to be allowed; got {diags:?}"
    );
}

#[test]
fn annotation_constant_fields_are_static() {
    let src = r#"
@interface A {
    int X = 1;
}
class C {
    static int m() {
        return A.X;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.code.as_ref() != "static-context" && d.code.as_ref() != "unresolved-field"),
        "expected annotation constant access to resolve as a static field; got {diags:?}"
    );
}

#[test]
fn var_without_initializer_reports_invalid_var() {
    let src = r#"
class C {
    void m() {
        var x;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "invalid-var"),
        "expected invalid-var diagnostic; got {diags:?}"
    );
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "var-requires-initializer"),
        "expected var-requires-initializer diagnostic; got {diags:?}"
    );
}

#[test]
fn var_initialized_to_null_reports_invalid_var() {
    let src = r#"
class C {
    void m() {
        var x = null;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "invalid-var"),
        "expected invalid-var diagnostic; got {diags:?}"
    );
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "var-null-initializer"),
        "expected var-null-initializer diagnostic; got {diags:?}"
    );
}

#[test]
fn foreach_var_infers_element_type_for_array() {
    let src = r#"
class C {
    void m(String[] xs) {
        for (var x : xs) {
            x.substring(1);
        }
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected foreach var element type to be inferred; got {diags:?}"
    );
}

#[test]
fn foreach_var_infers_element_type_for_array_initializer() {
    let src = r#"
class C {
    void m() {
        for (var s : new String[]{"a"}) {
            s.substring(1);
        }
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected foreach var element type to be inferred from array initializer; got {diags:?}"
    );
}

#[test]
fn resolve_method_call_demand_resolves_single_call_without_typeck_body() {
    let src = r#"
class C {
    String m() {
        return "x".substring(1);
    }
}
"#;

    let (db, file) = setup_db(src);

    // Find the call expression inside `C.m`.
    let tree = db.hir_item_tree(file);
    let (&m_ast_id, _) = tree
        .methods
        .iter()
        .find(|(_, method)| method.name == "m")
        .expect("expected method m");
    let m_id = nova_hir::ids::MethodId::new(file, m_ast_id);
    let body = db.hir_body(m_id);
    let call_expr = body
        .stmts
        .iter()
        .find_map(|(_, stmt)| match stmt {
            nova_hir::hir::Stmt::Return {
                expr: Some(expr), ..
            } => Some(*expr),
            _ => None,
        })
        .expect("expected return statement with expression");

    assert!(
        matches!(&body.exprs[call_expr], nova_hir::hir::Expr::Call { .. }),
        "expected return expression to be a Call"
    );

    let call_site = FileExprId {
        owner: DefWithBodyId::Method(m_id),
        expr: call_expr,
    };

    db.clear_query_stats();
    let resolved = db
        .resolve_method_call_demand(file, call_site)
        .expect("expected method call resolution");

    assert_eq!(resolved.name, "substring");

    // `substring` return type should be `String` (from the minimal-JDK stub env).
    let types = TypeStore::with_minimal_jdk();
    assert_eq!(
        resolved.return_type,
        Type::class(types.well_known().string, vec![])
    );

    let stats = db.query_stats();
    let typeck_body_activity = stats
        .by_query
        .get("typeck_body")
        .map(|s| (s.executions, s.validated_memoized))
        .unwrap_or((0, 0));
    assert_eq!(
        typeck_body_activity,
        (0, 0),
        "resolve_method_call_demand should not invoke full-body type checking"
    );
}

#[test]
fn resolve_method_call_demand_resolves_varargs_method_call() {
    let src = r#"
class C {
    static void foo(int... xs) {}
    static void m() {
        foo(1, 2, 3);
    }
}
"#;

    let (db, file) = setup_db(src);

    // Find the call expression inside `C.m`.
    let tree = db.hir_item_tree(file);
    let (&m_ast_id, _) = tree
        .methods
        .iter()
        .find(|(_, method)| method.name == "m")
        .expect("expected method m");
    let m_id = nova_hir::ids::MethodId::new(file, m_ast_id);
    let body = db.hir_body(m_id);
    let call_expr = body
        .stmts
        .iter()
        .find_map(|(_, stmt)| match stmt {
            nova_hir::hir::Stmt::Expr { expr, .. } => Some(*expr),
            _ => None,
        })
        .expect("expected expression statement with call");

    assert!(
        matches!(&body.exprs[call_expr], nova_hir::hir::Expr::Call { .. }),
        "expected statement expression to be a Call"
    );

    let call_site = FileExprId {
        owner: DefWithBodyId::Method(m_id),
        expr: call_expr,
    };

    db.clear_query_stats();
    let resolved = db
        .resolve_method_call_demand(file, call_site)
        .expect("expected varargs method call resolution");

    assert_eq!(resolved.name, "foo");
    assert!(resolved.is_varargs);
    assert!(resolved.used_varargs);
    assert_eq!(
        resolved.params,
        vec![
            Type::Primitive(PrimitiveType::Int),
            Type::Primitive(PrimitiveType::Int),
            Type::Primitive(PrimitiveType::Int),
        ]
    );
    assert_eq!(
        resolved.signature_params,
        Some(vec![Type::Array(Box::new(Type::Primitive(PrimitiveType::Int)))])
    );

    let stats = db.query_stats();
    let typeck_body_activity = stats
        .by_query
        .get("typeck_body")
        .map(|s| (s.executions, s.validated_memoized))
        .unwrap_or((0, 0));
    assert_eq!(
        typeck_body_activity,
        (0, 0),
        "resolve_method_call_demand should not invoke full-body type checking"
    );
}

#[test]
fn type_at_offset_display_does_not_execute_typeck_body() {
    let src = r#"
class C {
    String m() {
        return "x".substring(1);
    }
}
"#;

    let (db, file) = setup_db(src);
    db.clear_query_stats();

    let offset = src
        .find("substring(")
        .expect("snippet should contain substring call")
        + "substring".len();
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "String");

    let stats = db.query_stats();
    let typeck_body_activity = stats
        .by_query
        .get("typeck_body")
        .map(|s| (s.executions, s.validated_memoized))
        .unwrap_or((0, 0));
    assert_eq!(
        typeck_body_activity,
        (0, 0),
        "type_at_offset_display should not invoke full-body type checking"
    );
}

#[test]
fn resolve_method_call_demand_uses_expected_return_for_target_typed_generic_call() {
    let src = r#"
import java.util.*;
class C {
    List<String> m() {
        return Collections.emptyList();
    }
}
"#;

    let (db, file) = setup_db(src);

    // Find the call expression inside `C.m`.
    let tree = db.hir_item_tree(file);
    let (&m_ast_id, _) = tree
        .methods
        .iter()
        .find(|(_, method)| method.name == "m")
        .expect("expected method m");
    let m_id = nova_hir::ids::MethodId::new(file, m_ast_id);
    let body = db.hir_body(m_id);
    let call_expr = body
        .stmts
        .iter()
        .find_map(|(_, stmt)| match stmt {
            nova_hir::hir::Stmt::Return {
                expr: Some(expr), ..
            } => Some(*expr),
            _ => None,
        })
        .expect("expected return statement with expression");

    assert!(
        matches!(&body.exprs[call_expr], nova_hir::hir::Expr::Call { .. }),
        "expected return expression to be a Call"
    );

    let call_site = FileExprId {
        owner: DefWithBodyId::Method(m_id),
        expr: call_expr,
    };

    db.clear_query_stats();
    let resolved = db
        .resolve_method_call_demand(file, call_site)
        .expect("expected method call resolution");

    assert_eq!(resolved.name, "emptyList");

    // Target typing should infer `T = String`, yielding a `List<String>` return type.
    let types = TypeStore::with_minimal_jdk();
    let list = types
        .class_id("java.util.List")
        .expect("minimal jdk should define java.util.List");
    let string = Type::class(types.well_known().string, vec![]);
    assert_eq!(resolved.inferred_type_args, vec![string.clone()]);
    assert_eq!(resolved.return_type, Type::class(list, vec![string]));

    let stats = db.query_stats();
    let typeck_body_activity = stats
        .by_query
        .get("typeck_body")
        .map(|s| (s.executions, s.validated_memoized))
        .unwrap_or((0, 0));
    assert_eq!(
        typeck_body_activity,
        (0, 0),
        "resolve_method_call_demand should not invoke full-body type checking"
    );
}

#[test]
fn resolve_method_call_demand_propagates_expected_return_through_conditional() {
    let src = r#"
import java.util.*;
class C {
    List<String> m(boolean b) {
        return b ? Collections.emptyList() : Collections.emptyList();
    }
}
"#;

    let (db, file) = setup_db(src);

    // Find the call expression inside `C.m` (nested in a conditional return expr).
    let tree = db.hir_item_tree(file);
    let (&m_ast_id, _) = tree
        .methods
        .iter()
        .find(|(_, method)| method.name == "m")
        .expect("expected method m");
    let m_id = nova_hir::ids::MethodId::new(file, m_ast_id);
    let body = db.hir_body(m_id);

    let return_expr = body
        .stmts
        .iter()
        .find_map(|(_, stmt)| match stmt {
            nova_hir::hir::Stmt::Return {
                expr: Some(expr), ..
            } => Some(*expr),
            _ => None,
        })
        .expect("expected return statement with expression");

    let call_expr = match &body.exprs[return_expr] {
        nova_hir::hir::Expr::Conditional { then_expr, .. } => *then_expr,
        other => panic!("expected return expression to be Conditional, got {other:?}"),
    };

    assert!(
        matches!(&body.exprs[call_expr], nova_hir::hir::Expr::Call { .. }),
        "expected then-branch expression to be a Call"
    );

    let call_site = FileExprId {
        owner: DefWithBodyId::Method(m_id),
        expr: call_expr,
    };

    db.clear_query_stats();
    let resolved = db
        .resolve_method_call_demand(file, call_site)
        .expect("expected method call resolution");

    assert_eq!(resolved.name, "emptyList");

    // Target typing should infer `T = String`, yielding a `List<String>` return type.
    let types = TypeStore::with_minimal_jdk();
    let list = types
        .class_id("java.util.List")
        .expect("minimal jdk should define java.util.List");
    let string = Type::class(types.well_known().string, vec![]);
    assert_eq!(resolved.inferred_type_args, vec![string.clone()]);
    assert_eq!(resolved.return_type, Type::class(list, vec![string]));

    let stats = db.query_stats();
    let typeck_body_activity = stats
        .by_query
        .get("typeck_body")
        .map(|s| (s.executions, s.validated_memoized))
        .unwrap_or((0, 0));
    assert_eq!(
        typeck_body_activity,
        (0, 0),
        "resolve_method_call_demand should not invoke full-body type checking"
    );
}

#[test]
fn resolve_method_call_demand_uses_assignment_lhs_type_for_target_typed_generic_call() {
    let src = r#"
import java.util.*;
class C {
    void m(List<String> xs) {
        xs = Collections.emptyList();
    }
}
"#;

    let (db, file) = setup_db(src);

    // Find the call expression on the RHS of the assignment `xs = Collections.emptyList()`.
    let tree = db.hir_item_tree(file);
    let (&m_ast_id, _) = tree
        .methods
        .iter()
        .find(|(_, method)| method.name == "m")
        .expect("expected method m");
    let m_id = nova_hir::ids::MethodId::new(file, m_ast_id);
    let body = db.hir_body(m_id);

    let assign_expr = body
        .stmts
        .iter()
        .find_map(|(_, stmt)| match stmt {
            nova_hir::hir::Stmt::Expr { expr, .. } => Some(*expr),
            _ => None,
        })
        .expect("expected expression statement");

    let call_expr = match &body.exprs[assign_expr] {
        nova_hir::hir::Expr::Assign {
            op: nova_hir::hir::AssignOp::Assign,
            rhs,
            ..
        } => *rhs,
        other => panic!("expected assignment expression, got {other:?}"),
    };

    assert!(
        matches!(&body.exprs[call_expr], nova_hir::hir::Expr::Call { .. }),
        "expected RHS expression to be a Call"
    );

    let call_site = FileExprId {
        owner: DefWithBodyId::Method(m_id),
        expr: call_expr,
    };

    db.clear_query_stats();
    let resolved = db
        .resolve_method_call_demand(file, call_site)
        .expect("expected method call resolution");

    assert_eq!(resolved.name, "emptyList");

    // The assignment LHS `xs` has type `List<String>`, which should target-type the call.
    let types = TypeStore::with_minimal_jdk();
    let list = types
        .class_id("java.util.List")
        .expect("minimal jdk should define java.util.List");
    let string = Type::class(types.well_known().string, vec![]);
    assert_eq!(resolved.inferred_type_args, vec![string.clone()]);
    assert_eq!(resolved.return_type, Type::class(list, vec![string]));

    let stats = db.query_stats();
    let typeck_body_activity = stats
        .by_query
        .get("typeck_body")
        .map(|s| (s.executions, s.validated_memoized))
        .unwrap_or((0, 0));
    assert_eq!(
        typeck_body_activity,
        (0, 0),
        "resolve_method_call_demand should not invoke full-body type checking"
    );
}

#[test]
fn resolve_method_call_demand_targets_lambda_block_from_typed_initializer() {
    let src = r#"
import java.util.function.Function;
class C {
    void m() {
        Function<String, String> f = s -> { return s.substring(1); };
    }
}
"#;

    let (db, file) = setup_db(src);

    // Find the `substring` call expression inside the lambda block.
    let tree = db.hir_item_tree(file);
    let (&m_ast_id, _) = tree
        .methods
        .iter()
        .find(|(_, method)| method.name == "m")
        .expect("expected method m");
    let m_id = nova_hir::ids::MethodId::new(file, m_ast_id);
    let body = db.hir_body(m_id);

    let lambda_expr = body
        .stmts
        .iter()
        .find_map(|(_, stmt)| match stmt {
            nova_hir::hir::Stmt::Let {
                initializer: Some(expr),
                ..
            } => Some(*expr),
            _ => None,
        })
        .expect("expected local initializer");

    let lambda_body = match &body.exprs[lambda_expr] {
        nova_hir::hir::Expr::Lambda { body, .. } => body.clone(),
        other => panic!("expected lambda initializer, got {other:?}"),
    };

    let block_stmt = match lambda_body {
        nova_hir::hir::LambdaBody::Block(stmt) => stmt,
        other => panic!("expected block-bodied lambda, got {other:?}"),
    };

    let call_expr = match &body.stmts[block_stmt] {
        nova_hir::hir::Stmt::Block { statements, .. } => statements
            .iter()
            .find_map(|stmt| match &body.stmts[*stmt] {
                nova_hir::hir::Stmt::Return {
                    expr: Some(expr), ..
                } => Some(*expr),
                _ => None,
            })
            .expect("expected return statement in lambda body"),
        other => panic!("expected lambda body block, got {other:?}"),
    };

    assert!(
        matches!(&body.exprs[call_expr], nova_hir::hir::Expr::Call { .. }),
        "expected lambda return expression to be a Call"
    );

    let call_site = FileExprId {
        owner: DefWithBodyId::Method(m_id),
        expr: call_expr,
    };

    db.clear_query_stats();
    let resolved = db
        .resolve_method_call_demand(file, call_site)
        .expect("expected method call resolution");

    assert_eq!(resolved.name, "substring");

    // `substring` return type should be `String` (from the minimal-JDK stub env).
    let types = TypeStore::with_minimal_jdk();
    assert_eq!(
        resolved.return_type,
        Type::class(types.well_known().string, vec![])
    );

    let stats = db.query_stats();
    let typeck_body_activity = stats
        .by_query
        .get("typeck_body")
        .map(|s| (s.executions, s.validated_memoized))
        .unwrap_or((0, 0));
    assert_eq!(
        typeck_body_activity,
        (0, 0),
        "resolve_method_call_demand should not invoke full-body type checking"
    );
}

#[test]
fn resolve_method_call_demand_targets_lambda_block_from_call_argument() {
    let src = r#"
import java.util.function.Function;
class C {
    static void use(Function<String, String> f) { }
    void m() {
        use(s -> { return s.substring(1); });
    }
}
"#;

    let (db, file) = setup_db(src);

    // Find the `substring` call expression inside the lambda block.
    let tree = db.hir_item_tree(file);
    let (&m_ast_id, _) = tree
        .methods
        .iter()
        .find(|(_, method)| method.name == "m")
        .expect("expected method m");
    let m_id = nova_hir::ids::MethodId::new(file, m_ast_id);
    let body = db.hir_body(m_id);

    let use_call = body
        .stmts
        .iter()
        .find_map(|(_, stmt)| match stmt {
            nova_hir::hir::Stmt::Expr { expr, .. } => Some(*expr),
            _ => None,
        })
        .expect("expected expression statement");
    let lambda_expr = match &body.exprs[use_call] {
        nova_hir::hir::Expr::Call { args, .. } => args.first().copied().expect("expected arg"),
        other => panic!("expected call expression statement, got {other:?}"),
    };

    let block_stmt = match &body.exprs[lambda_expr] {
        nova_hir::hir::Expr::Lambda { body, .. } => match body {
            nova_hir::hir::LambdaBody::Block(stmt) => *stmt,
            other => panic!("expected block-bodied lambda, got {other:?}"),
        },
        other => panic!("expected lambda argument, got {other:?}"),
    };

    let call_expr = match &body.stmts[block_stmt] {
        nova_hir::hir::Stmt::Block { statements, .. } => statements
            .iter()
            .find_map(|stmt| match &body.stmts[*stmt] {
                nova_hir::hir::Stmt::Return {
                    expr: Some(expr), ..
                } => Some(*expr),
                _ => None,
            })
            .expect("expected return statement in lambda body"),
        other => panic!("expected lambda body block, got {other:?}"),
    };

    assert!(
        matches!(&body.exprs[call_expr], nova_hir::hir::Expr::Call { .. }),
        "expected lambda return expression to be a Call"
    );

    let call_site = FileExprId {
        owner: DefWithBodyId::Method(m_id),
        expr: call_expr,
    };

    db.clear_query_stats();
    let resolved = db
        .resolve_method_call_demand(file, call_site)
        .expect("expected method call resolution");

    assert_eq!(resolved.name, "substring");

    // `substring` return type should be `String` (from the minimal-JDK stub env).
    let types = TypeStore::with_minimal_jdk();
    assert_eq!(
        resolved.return_type,
        Type::class(types.well_known().string, vec![])
    );

    let stats = db.query_stats();
    let typeck_body_activity = stats
        .by_query
        .get("typeck_body")
        .map(|s| (s.executions, s.validated_memoized))
        .unwrap_or((0, 0));
    assert_eq!(
        typeck_body_activity,
        (0, 0),
        "resolve_method_call_demand should not invoke full-body type checking"
    );
}

#[test]
fn resolve_method_call_demand_resolves_static_import_from_other_file() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();
    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let foo_file = FileId::from_raw(1);
    let use_file = FileId::from_raw(2);

    let src_foo = r#"
package p;
class Foo {
    static int bar() { return 0; }
}
"#;
    let src_use = r#"
package p;
import static p.Foo.bar;
class Use {
    int m() { return bar(); }
}
"#;

    set_file(&mut db, project, foo_file, "src/p/Foo.java", src_foo);
    set_file(&mut db, project, use_file, "src/p/Use.java", src_use);
    db.set_project_files(project, Arc::new(vec![foo_file, use_file]));

    // Find the call expression inside `Use.m`.
    let tree = db.hir_item_tree(use_file);
    let (&m_ast_id, _) = tree
        .methods
        .iter()
        .find(|(_, method)| method.name == "m")
        .expect("expected method m");
    let m_id = nova_hir::ids::MethodId::new(use_file, m_ast_id);
    let body = db.hir_body(m_id);
    let call_expr = body
        .stmts
        .iter()
        .find_map(|(_, stmt)| match stmt {
            nova_hir::hir::Stmt::Return {
                expr: Some(expr), ..
            } => Some(*expr),
            _ => None,
        })
        .expect("expected return statement with expression");

    assert!(
        matches!(&body.exprs[call_expr], nova_hir::hir::Expr::Call { .. }),
        "expected return expression to be a Call"
    );

    let call_site = FileExprId {
        owner: DefWithBodyId::Method(m_id),
        expr: call_expr,
    };

    db.clear_query_stats();
    let resolved = db
        .resolve_method_call_demand(use_file, call_site)
        .expect("expected method call resolution");
    assert_eq!(resolved.name, "bar");
    assert_eq!(resolved.return_type, Type::Primitive(PrimitiveType::Int));

    let stats = db.query_stats();
    let typeck_body_activity = stats
        .by_query
        .get("typeck_body")
        .map(|s| (s.executions, s.validated_memoized))
        .unwrap_or((0, 0));
    assert_eq!(
        typeck_body_activity,
        (0, 0),
        "resolve_method_call_demand should not invoke full-body type checking"
    );
}

#[test]
fn resolve_method_call_demand_resolves_inherited_method_via_extends() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let base_file = FileId::from_raw(1);
    let derived_file = FileId::from_raw(2);
    let use_file = FileId::from_raw(3);

    set_file(
        &mut db,
        project,
        base_file,
        "src/p/Base.java",
        r#"package p; class Base { String foo() { return ""; } }"#,
    );
    set_file(
        &mut db,
        project,
        derived_file,
        "src/p/Derived.java",
        r#"package p; class Derived extends Base {}"#,
    );
    let src_use = r#"package p; class Use { String m() { return new Derived().foo(); } }"#;
    set_file(&mut db, project, use_file, "src/p/Use.java", src_use);
    db.set_project_files(project, Arc::new(vec![base_file, derived_file, use_file]));

    // Find the call expression inside `Use.m`.
    let tree = db.hir_item_tree(use_file);
    let (&m_ast_id, _) = tree
        .methods
        .iter()
        .find(|(_, method)| method.name == "m")
        .expect("expected method m");
    let m_id = nova_hir::ids::MethodId::new(use_file, m_ast_id);
    let body = db.hir_body(m_id);
    let call_expr = body
        .stmts
        .iter()
        .find_map(|(_, stmt)| match stmt {
            nova_hir::hir::Stmt::Return {
                expr: Some(expr), ..
            } => Some(*expr),
            _ => None,
        })
        .expect("expected return statement with expression");

    assert!(
        matches!(&body.exprs[call_expr], nova_hir::hir::Expr::Call { .. }),
        "expected return expression to be a Call"
    );

    let call_site = FileExprId {
        owner: DefWithBodyId::Method(m_id),
        expr: call_expr,
    };

    db.clear_query_stats();
    let resolved = db
        .resolve_method_call_demand(use_file, call_site)
        .expect("expected inherited method call resolution");

    assert_eq!(resolved.name, "foo");

    let types = TypeStore::with_minimal_jdk();
    assert_eq!(
        resolved.return_type,
        Type::class(types.well_known().string, vec![])
    );

    let stats = db.query_stats();
    let typeck_body_activity = stats
        .by_query
        .get("typeck_body")
        .map(|s| (s.executions, s.validated_memoized))
        .unwrap_or((0, 0));
    assert_eq!(
        typeck_body_activity,
        (0, 0),
        "resolve_method_call_demand should not invoke full-body type checking"
    );
}

#[test]
fn resolve_method_call_demand_resolves_inherited_interface_method_via_extends() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let i_file = FileId::from_raw(1);
    let j_file = FileId::from_raw(2);
    let use_file = FileId::from_raw(3);

    set_file(
        &mut db,
        project,
        i_file,
        "src/p/I.java",
        r#"package p; interface I { int foo(); }"#,
    );
    set_file(
        &mut db,
        project,
        j_file,
        "src/p/J.java",
        r#"package p; interface J extends I {}"#,
    );
    let src_use = r#"package p; class Use { int m(J j) { return j.foo(); } }"#;
    set_file(&mut db, project, use_file, "src/p/Use.java", src_use);
    db.set_project_files(project, Arc::new(vec![i_file, j_file, use_file]));

    // Find the call expression inside `Use.m`.
    let tree = db.hir_item_tree(use_file);
    let (&m_ast_id, _) = tree
        .methods
        .iter()
        .find(|(_, method)| method.name == "m")
        .expect("expected method m");
    let m_id = nova_hir::ids::MethodId::new(use_file, m_ast_id);
    let body = db.hir_body(m_id);
    let call_expr = body
        .stmts
        .iter()
        .find_map(|(_, stmt)| match stmt {
            nova_hir::hir::Stmt::Return {
                expr: Some(expr), ..
            } => Some(*expr),
            _ => None,
        })
        .expect("expected return statement with expression");

    assert!(
        matches!(&body.exprs[call_expr], nova_hir::hir::Expr::Call { .. }),
        "expected return expression to be a Call"
    );

    let call_site = FileExprId {
        owner: DefWithBodyId::Method(m_id),
        expr: call_expr,
    };

    db.clear_query_stats();
    let resolved = db
        .resolve_method_call_demand(use_file, call_site)
        .expect("expected inherited interface method call resolution");

    assert_eq!(resolved.name, "foo");
    assert_eq!(resolved.return_type, Type::Primitive(PrimitiveType::Int));

    let stats = db.query_stats();
    let typeck_body_activity = stats
        .by_query
        .get("typeck_body")
        .map(|s| (s.executions, s.validated_memoized))
        .unwrap_or((0, 0));
    assert_eq!(
        typeck_body_activity,
        (0, 0),
        "resolve_method_call_demand should not invoke full-body type checking"
    );
}

#[test]
fn resolve_method_call_demand_does_not_load_java_types_from_classpath_stubs() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();
    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));

    // Create a classpath index that (incorrectly) contains a `java.*` class. The resolver should
    // ignore these (mirroring JVM restrictions), and demand-driven type checking should not be
    // able to "rescue" the type by lazily loading it from the classpath.
    let foo_stub = nova_classpath::ClasspathClassStub {
        binary_name: "java.fake.Foo".to_string(),
        internal_name: "java/fake/Foo".to_string(),
        access_flags: 0,
        super_binary_name: None,
        interfaces: Vec::new(),
        signature: None,
        annotations: Vec::new(),
        fields: Vec::new(),
        methods: vec![nova_classpath::ClasspathMethodStub {
            name: "bar".to_string(),
            descriptor: "()V".to_string(),
            signature: None,
            access_flags: 0,
            annotations: Vec::new(),
        }],
    };

    let module_aware_index =
        nova_classpath::ModuleAwareClasspathIndex::from_stubs(vec![(foo_stub, None)]);
    let classpath_index = module_aware_index.types.clone();
    db.set_classpath_index(project, Some(ArcEq::new(Arc::new(classpath_index))));

    let src = r#"
class C {
  void m() {
    java.fake.Foo f = null;
    f.bar();
  }
}
"#;

    let file = FileId::from_raw(1);
    set_file(&mut db, project, file, "src/Test.java", src);
    db.set_project_files(project, Arc::new(vec![file]));

    // Find the call expression inside `C.m`.
    let tree = db.hir_item_tree(file);
    let (&m_ast_id, _) = tree
        .methods
        .iter()
        .find(|(_, method)| method.name == "m")
        .expect("expected method m");
    let m_id = nova_hir::ids::MethodId::new(file, m_ast_id);
    let body = db.hir_body(m_id);
    let call_expr = body
        .stmts
        .iter()
        .find_map(|(_, stmt)| match stmt {
            nova_hir::hir::Stmt::Expr { expr, .. } => Some(*expr),
            _ => None,
        })
        .expect("expected expression statement");

    assert!(
        matches!(&body.exprs[call_expr], nova_hir::hir::Expr::Call { .. }),
        "expected expression to be a Call"
    );

    let call_site = FileExprId {
        owner: DefWithBodyId::Method(m_id),
        expr: call_expr,
    };

    db.clear_query_stats();
    let resolved = db.resolve_method_call_demand(file, call_site);
    assert!(
        resolved.is_none(),
        "expected demand call resolution to fail for java.fake.Foo.bar (should not load java.* from classpath stubs), got {resolved:?}"
    );

    let stats = db.query_stats();
    let typeck_body_activity = stats
        .by_query
        .get("typeck_body")
        .map(|s| (s.executions, s.validated_memoized))
        .unwrap_or((0, 0));
    assert_eq!(
        typeck_body_activity,
        (0, 0),
        "resolve_method_call_demand should not invoke full-body type checking"
    );
}

#[test]
fn demand_var_self_initializer_emits_cyclic_var_and_does_not_overflow() {
    let src = r#"
class C {
    void m() {
        var x = x;
    }
}
"#;

    let (db, file) = setup_db(src);
    let owner = first_method_with_body(&db, file);
    let body = match owner {
        DefWithBodyId::Method(m) => db.hir_body(m),
        DefWithBodyId::Constructor(c) => db.hir_constructor_body(c),
        DefWithBodyId::Initializer(i) => db.hir_initializer_body(i),
    };

    let init_expr = body
        .stmts
        .iter()
        .find_map(|(_, stmt)| match stmt {
            nova_hir::hir::Stmt::Let {
                initializer: Some(init),
                ..
            } => Some(*init),
            _ => None,
        })
        .expect("expected a let statement with an initializer");

    // Reset query stats so the assertion below only reflects the `type_of_expr_demand_result` call.
    db.clear_query_stats();

    let res = db.type_of_expr_demand_result(
        file,
        FileExprId {
            owner,
            expr: init_expr,
        },
    );
    assert!(
        res.diagnostics
            .iter()
            .any(|d| d.code.as_ref() == "cyclic-var"),
        "expected cyclic-var diagnostic for `var x = x;`, got {:?}",
        res.diagnostics
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
fn source_varargs_method_call_resolves_in_static_context() {
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
        "expected varargs call to resolve in static context, got {diags:?}"
    );
}

#[test]
fn source_varargs_method_call_resolves_with_single_arg() {
    let src = r#"
class C {
    static void foo(int... xs) {}
    void m() { foo(1); }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected varargs method call with one arg to resolve, got {diags:?}"
    );
}

#[test]
fn source_varargs_method_call_resolves_with_no_args() {
    let src = r#"
class C {
    static void foo(int... xs) {}
    void m() { foo(); }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected varargs method call with no args to resolve, got {diags:?}"
    );
}

#[test]
fn source_varargs_constructor_is_tagged() {
    let src = r#"
class Foo {
    Foo(int... xs) {}
}
"#;

    let (db, file) = setup_db(src);

    let tree = db.hir_item_tree(file);
    let (&ctor_ast_id, _) = tree
        .constructors
        .iter()
        .next()
        .expect("expected constructor in item tree");
    let ctor_id = nova_hir::ids::ConstructorId::new(file, ctor_ast_id);

    let result = db.typeck_body(DefWithBodyId::Constructor(ctor_id));
    let env = &*result.env;
    let foo = env.lookup_class("Foo").expect("expected Foo to be in env");
    let def = env.class(foo).expect("expected Foo class def");
    assert_eq!(def.constructors.len(), 1, "expected one constructor");
    assert!(
        def.constructors[0].is_varargs,
        "expected varargs constructor to be tagged as varargs"
    );
}

#[test]
fn ensure_workspace_class_preserves_constructor_defs() {
    let src = r#"
class Foo {
    Foo(int... xs) {}
    void bar() {}
}

class Use {
    void m() {
        Foo f = null;
        f.bar();
    }
}
"#;

    let (db, file) = setup_db(src);

    let tree = db.hir_item_tree(file);
    let (&m_ast_id, _) = tree
        .methods
        .iter()
        .find(|(_, method)| method.name == "m")
        .expect("expected method m");
    let m_id = nova_hir::ids::MethodId::new(file, m_ast_id);

    let result = db.typeck_body(DefWithBodyId::Method(m_id));
    let env = &*result.env;
    let foo = env.lookup_class("Foo").expect("expected Foo to be in env");
    let def = env.class(foo).expect("expected Foo class def");
    assert_eq!(def.constructors.len(), 1, "expected one constructor");
    assert!(
        def.constructors[0].is_varargs,
        "expected Foo(int... xs) constructor to remain tagged as varargs"
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
fn target_typing_infers_generic_method_return_from_typed_initializer() {
    let src = r#"
import java.util.*;
class C {
    void m() {
        List<String> xs = Collections.emptyList();
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"
            && d.code.as_ref() != "type-mismatch"),
        "expected target-typed local initializer to resolve without unresolved-method/type-mismatch, got {diags:?}"
    );

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
fn explicit_method_type_args_infer_generic_return_without_target_type() {
    let src = r#"
 import java.util.*;
class C {
    void m() {
        Collections.<String>emptyList();
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
fn target_typing_infers_generic_method_return_from_call_argument() {
    let src = r#"
 import java.util.*;
class C {
    static void take(List<String> xs) {}
    void m() {
        take(Collections.emptyList());
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"
            && d.code.as_ref() != "type-mismatch"),
        "expected target typing through invocation context to avoid unresolved-method/type-mismatch, got {diags:?}"
    );
}

#[test]
fn target_typing_infers_diamond_new_expr_from_call_argument() {
    let src = r#"
 import java.util.*;
class C {
    static void take(List<String> xs) {}
    void m() {
        take(new ArrayList<>());
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"
            && d.code.as_ref() != "unresolved-constructor"
            && d.code.as_ref() != "type-mismatch"),
        "expected diamond to be target-typed via invocation context, got {diags:?}"
    );
}

#[test]
fn diamond_inference_uses_target_type_from_return() {
    let src = r#"
 import java.util.*;
class C {
    List<String> m() {
        return new ArrayList<>();
    }
}
"#;

    let (db, file) = setup_db(src);
    let offset = src
        .find("ArrayList<>")
        .expect("snippet should contain ArrayList diamond")
        + "ArrayList".len();
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "ArrayList<String>");
}

#[test]
fn diamond_inference_with_constructor_args_resolves_constructor_and_uses_target_type() {
    let src = r#"
 import java.util.*;
class C {
    List<String> m() {
        return new ArrayList<>(1);
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.code.as_ref() != "unresolved-constructor"),
        "expected ArrayList(int) ctor to resolve in minimal JDK; got {diags:?}"
    );

    let offset = src
        .find("ArrayList<>")
        .expect("snippet should contain ArrayList diamond")
        + "ArrayList".len();
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "ArrayList<String>");
}

#[test]
fn diamond_inference_uses_target_type_from_call_argument() {
    let src = r#"
import java.util.*;
class C {
    void take(List<String> xs) {}
    void m() {
        take(new ArrayList<>());
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"
            && d.code.as_ref() != "unchecked"
            && d.code.as_ref() != "unresolved-constructor"),
        "expected call argument diamond to target-type without warnings; got {diags:?}"
    );

    let offset = src
        .find("ArrayList<>")
        .expect("snippet should contain ArrayList diamond")
        + "ArrayList".len();
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "ArrayList<String>");
}

#[test]
fn diamond_inference_uses_target_type_from_constructor_argument() {
    let src = r#"
import java.util.*;
class Foo { Foo(List<String> xs) {} }
class C {
    void m() {
        new Foo(new ArrayList<>());
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-constructor"
            && d.code.as_ref() != "unresolved-method"
            && d.code.as_ref() != "unchecked"),
        "expected ctor-arg diamond to target-type without warnings; got {diags:?}"
    );

    let offset = src
        .find("ArrayList<>")
        .expect("snippet should contain ArrayList diamond")
        + "ArrayList".len();
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "ArrayList<String>");
}

#[test]
fn diamond_inference_for_var_defaults_to_object() {
    let src = r#"
 import java.util.*;
class C {
    void m() {
        var xs = new ArrayList<>();
        xs.add("x");
    }
}
"#;

    let (db, file) = setup_db(src);
    let offset = src.find("xs.add").expect("snippet should contain xs.add") + 1;
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "ArrayList<Object>");
}

#[test]
fn static_imported_jdk_method_resolves_and_infers_return_type() {
    let src = r#"
import java.util.List;
import static java.util.Collections.emptyList;
class C {
    List<String> m() {
        return emptyList();
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"
            && d.code.as_ref() != "unresolved-static-member"
            && d.code.as_ref() != "unresolved-type"),
        "expected static-imported emptyList() to resolve without unresolved diagnostics, got {diags:?}"
    );

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
fn unresolved_unqualified_method_call_diagnostic_includes_candidates() {
    let src = r#"
class C {
    void foo() {}
    void m() {
        foo(1);
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
        diag.message.contains("foo") && diag.message.contains("wrong arity"),
        "expected unresolved-method diagnostic to include candidate arity info, got {:?}",
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
fn type_at_offset_finds_synchronized_lock_expr() {
    let src = r#"
class C {
    void m() {
        Object x = new Object();
        synchronized (x) { }
    }
}
"#;

    let (db, file) = setup_db(src);
    let sync = src
        .find("synchronized (x)")
        .expect("snippet should contain synchronized statement");
    let offset = sync + "synchronized (".len();
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "Object");
}

#[test]
fn extends_allows_inherited_method_call() {
    let src = r#"
class A { int foo(){ return 1; } }
class B extends A { int m(){ return foo(); } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected inherited method call to resolve via extends; got {diags:?}"
    );
}

#[test]
fn extends_allows_inherited_field_access() {
    let src = r#"
class A { int x = 1; }
class B extends A { int m(){ return x; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-field"),
        "expected inherited field access to resolve via extends; got {diags:?}"
    );
}

#[test]
fn extends_makes_super_type_precise() {
    let src = r#"
class A { int foo(){ return 1; } }
class B extends A { int m(){ return super.foo(); } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected super.foo() to resolve via extends; got {diags:?}"
    );

    let offset = src.find("super").expect("snippet should contain `super`") + 1;
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "A");
}

#[test]
fn object_methods_resolve_even_if_extends_unresolved() {
    let src = r#"
class C extends Missing {
    void m() {
        this.toString();
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| !(d.code.as_ref() == "unresolved-method" && d.message.contains("toString"))),
        "expected Object methods to remain available even with an unresolved superclass; got {diags:?}"
    );
}

#[test]
fn extends_keeps_inherited_members_when_type_arg_unresolved() {
    let src = r#"
import java.util.*;
class C extends ArrayList<Missing> {
    void m() {
        get(0);
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected inherited ArrayList.get(int) (via List) to resolve even if type argument is unresolved; got {diags:?}"
    );
}

#[test]
fn implements_allows_interface_method_lookup() {
    let src = r#"
interface I { int foo(); }
class C implements I { int m(){ return foo(); } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected interface method lookup to resolve via implements; got {diags:?}"
    );
}

#[test]
fn this_types_as_enclosing_class() {
    let src = r#"
class C {
    int x;
    void m() {
        int y = this.x;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-field"),
        "expected `this` to type as the enclosing class and resolve fields; got {diags:?}"
    );
}

#[test]
fn qualified_this_types_as_qualifier_and_resolves_fields() {
    let src = r#"
class Outer {
    int x;
    class Inner {
        int m() {
            return Outer.this.x;
        }
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-field"),
        "expected `Outer.this` to type as `Outer` and resolve `x`; got {diags:?}"
    );

    let offset = src.find("this").expect("snippet should contain `this`") + 1;
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "Outer");
}

#[test]
fn super_types_as_declared_superclass() {
    let src = r#"
class Base { void foo() {} }
class C extends Base {
    void m() { super.foo(); }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected `super.foo()` to resolve against the declared superclass; got {diags:?}"
    );

    let offset = src.find("super").expect("snippet should contain `super`") + 1;
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "Base");
}

#[test]
fn qualified_super_types_as_declared_superclass() {
    let src = r#"
class Base { void foo() {} }
class Outer extends Base {
    class Inner {
        void m() { Outer.super.foo(); }
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected `Outer.super.foo()` to resolve against the declared superclass; got {diags:?}"
    );

    let offset = src.find("super").expect("snippet should contain `super`") + 1;
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "Base");
}

#[test]
fn this_in_static_method_is_error() {
    let src = r#"
class C {
    static void m() {
        this.toString();
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "this-in-static-context"),
        "expected `this-in-static-context` diagnostic; got {diags:?}"
    );
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected no unresolved-method diagnostics when `this` is used in a static method; got {diags:?}"
    );
}

#[test]
fn super_in_static_method_is_error() {
    let src = r#"
class C {
    static void m() {
        super.toString();
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "super-in-static-context"),
        "expected `super-in-static-context` diagnostic; got {diags:?}"
    );
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected no unresolved-method diagnostics when `super` is used in a static method; got {diags:?}"
    );
}

#[test]
fn this_in_static_method_diagnostic_mentions_static_context() {
    let src = r#"
class C {
  static Object m() {
    return this;
  }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.severity == Severity::Error && d.message.contains("static context")),
        "expected `this` in static context to produce an error mentioning static context; got {diags:?}"
    );
}

#[test]
fn super_in_static_method_diagnostic_mentions_static_context() {
    let src = r#"
class C {
  static Object m() {
    return super;
  }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.severity == Severity::Error && d.message.contains("static context")),
        "expected `super` in static context to produce an error mentioning static context; got {diags:?}"
    );
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
fn cross_file_type_ref_in_signature_does_not_emit_unresolved_type() {
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
        "package p; class A {}",
    );
    set_file(
        &mut db,
        project,
        b_file,
        "src/p/B.java",
        "package p; class B { void m(A a) {} }",
    );
    db.set_project_files(project, Arc::new(vec![a_file, b_file]));

    let diags = db.type_diagnostics(b_file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-type"),
        "expected `A` to resolve in a signature type reference, got {diags:?}"
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
fn cross_file_extends_allows_inherited_method_call() {
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
        "package p; class A { int foo(){ return 1; } }",
    );
    set_file(
        &mut db,
        project,
        b_file,
        "src/p/B.java",
        "package p; class B extends A { int m(){ return foo(); } }",
    );
    db.set_project_files(project, Arc::new(vec![a_file, b_file]));

    let diags = db.type_diagnostics(b_file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected cross-file inherited method call to resolve; got {diags:?}"
    );
}

#[test]
fn cross_file_extends_allows_inherited_field_access() {
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
        "package p; class A { int x = 1; }",
    );
    set_file(
        &mut db,
        project,
        b_file,
        "src/p/B.java",
        "package p; class B extends A { int m(){ return x; } }",
    );
    db.set_project_files(project, Arc::new(vec![a_file, b_file]));

    let diags = db.type_diagnostics(b_file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-field"),
        "expected cross-file inherited field access to resolve; got {diags:?}"
    );
}

#[test]
fn cross_file_implements_allows_interface_method_lookup() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let i_file = FileId::from_raw(1);
    let c_file = FileId::from_raw(2);

    set_file(
        &mut db,
        project,
        i_file,
        "src/p/I.java",
        "package p; interface I { int foo(); }",
    );
    set_file(
        &mut db,
        project,
        c_file,
        "src/p/C.java",
        "package p; class C implements I { int m(){ return foo(); } }",
    );
    db.set_project_files(project, Arc::new(vec![i_file, c_file]));

    let diags = db.type_diagnostics(c_file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected cross-file interface method lookup to resolve via implements; got {diags:?}"
    );
}

#[test]
fn cross_file_interface_extends_allows_inherited_method_lookup() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let i_file = FileId::from_raw(1);
    let j_file = FileId::from_raw(2);
    let use_file = FileId::from_raw(3);

    set_file(
        &mut db,
        project,
        i_file,
        "src/p/I.java",
        "package p; interface I { int foo(); }",
    );
    set_file(
        &mut db,
        project,
        j_file,
        "src/p/J.java",
        "package p; interface J extends I {}",
    );
    set_file(
        &mut db,
        project,
        use_file,
        "src/p/Use.java",
        "package p; class Use { int m(J j){ return j.foo(); } }",
    );
    db.set_project_files(project, Arc::new(vec![i_file, j_file, use_file]));

    let diags = db.type_diagnostics(use_file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected cross-file inherited interface method lookup to resolve; got {diags:?}"
    );
}

#[test]
fn cross_file_varargs_method_call_resolves_on_workspace_class() {
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

    let src_a = r#"package p; class A { void m(int... xs) {} }"#;
    let src_b = r#"package p; class B { void test() { new A().m(1, 2); } }"#;

    set_file(&mut db, project, a_file, "src/p/A.java", src_a);
    set_file(&mut db, project, b_file, "src/p/B.java", src_b);
    db.set_project_files(project, Arc::new(vec![a_file, b_file]));

    let diags = db.type_diagnostics(b_file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected cross-file varargs method call to resolve, got {diags:?}"
    );
}

#[test]
fn cross_file_inherited_method_call_resolves_via_extends() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let base_file = FileId::from_raw(1);
    let derived_file = FileId::from_raw(2);
    let use_file = FileId::from_raw(3);

    set_file(
        &mut db,
        project,
        base_file,
        "src/p/Base.java",
        r#"package p; class Base { String foo() { return ""; } }"#,
    );
    set_file(
        &mut db,
        project,
        derived_file,
        "src/p/Derived.java",
        r#"package p; class Derived extends Base {}"#,
    );
    let src_use = r#"package p; class Use { String m() { return new Derived().foo(); } }"#;
    set_file(&mut db, project, use_file, "src/p/Use.java", src_use);
    db.set_project_files(project, Arc::new(vec![base_file, derived_file, use_file]));

    let diags = db.type_diagnostics(use_file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected inherited workspace method call to resolve, got {diags:?}"
    );

    let offset = src_use
        .find("foo()")
        .expect("snippet should contain foo call")
        + "foo".len();
    let ty = db
        .type_at_offset_display(use_file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "String");
}

#[test]
fn cross_file_inherited_interface_method_call_resolves_via_extends() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let i_file = FileId::from_raw(1);
    let j_file = FileId::from_raw(2);
    let use_file = FileId::from_raw(3);

    set_file(
        &mut db,
        project,
        i_file,
        "src/p/I.java",
        r#"package p; interface I { int foo(); }"#,
    );
    set_file(
        &mut db,
        project,
        j_file,
        "src/p/J.java",
        r#"package p; interface J extends I {}"#,
    );
    let src_use = r#"package p; class Use { int m(J j) { return j.foo(); } }"#;
    set_file(&mut db, project, use_file, "src/p/Use.java", src_use);
    db.set_project_files(project, Arc::new(vec![i_file, j_file, use_file]));

    let diags = db.type_diagnostics(use_file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected inherited interface method call to resolve, got {diags:?}"
    );

    let offset = src_use
        .find("foo()")
        .expect("snippet should contain foo call")
        + "foo".len();
    let ty = db
        .type_at_offset_display(use_file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "int");
}

#[test]
fn cross_file_inherited_field_access_resolves_via_extends() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let base_file = FileId::from_raw(1);
    let derived_file = FileId::from_raw(2);
    let use_file = FileId::from_raw(3);

    set_file(
        &mut db,
        project,
        base_file,
        "src/p/Base.java",
        r#"package p; class Base { int x = 1; }"#,
    );
    set_file(
        &mut db,
        project,
        derived_file,
        "src/p/Derived.java",
        r#"package p; class Derived extends Base {}"#,
    );
    let src_use = r#"package p; class Use { int m() { return new Derived().x; } }"#;
    set_file(&mut db, project, use_file, "src/p/Use.java", src_use);
    db.set_project_files(project, Arc::new(vec![base_file, derived_file, use_file]));

    let offset = src_use
        .find(".x")
        .expect("snippet should contain `.x` field access")
        + 1;

    // `type_at_offset_display` is demand-driven and should not execute full-body type checking.
    db.clear_query_stats();
    let ty = db
        .type_at_offset_display(use_file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "int");

    let typeck_body_executions = db
        .query_stats()
        .by_query
        .get("typeck_body")
        .map(|s| s.executions)
        .unwrap_or(0);
    assert_eq!(
        typeck_body_executions, 0,
        "type_at_offset_display should not execute typeck_body"
    );

    let diags = db.type_diagnostics(use_file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-field"),
        "expected inherited field access to resolve; got {diags:?}"
    );
}

#[test]
fn cross_file_static_method_call_resolves_on_workspace_class() {
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

    let src_a = r#"package p; class A { static int foo(int x) { return x; } }"#;
    let src_b = r#"package p; class B { int m() { return A.foo(1); } }"#;

    set_file(&mut db, project, a_file, "src/p/A.java", src_a);
    set_file(&mut db, project, b_file, "src/p/B.java", src_b);
    db.set_project_files(project, Arc::new(vec![a_file, b_file]));

    let offset = src_b
        .find("foo(1)")
        .expect("snippet should contain foo call")
        + "foo".len();
    let ty = db
        .type_at_offset_display(b_file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "int");

    let diags = db.type_diagnostics(b_file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected cross-file workspace static method call to resolve, got {diags:?}"
    );
}

#[test]
fn static_import_resolves_workspace_members_across_files() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let util_file = FileId::from_raw(1);
    let use_file = FileId::from_raw(2);

    let src_util =
        r#"package p; class Util { static int F = 1; static int foo(int x) { return x; } }"#;
    let src_use = r#"package p; import static p.Util.*; class Use { int m() { return foo(F); } }"#;

    set_file(&mut db, project, util_file, "src/p/Util.java", src_util);
    set_file(&mut db, project, use_file, "src/p/Use.java", src_use);
    db.set_project_files(project, Arc::new(vec![util_file, use_file]));

    let offset = src_use
        .find("foo(F)")
        .expect("snippet should contain foo call")
        + "foo".len();
    let ty = db
        .type_at_offset_display(use_file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "int");

    let diags = db.type_diagnostics(use_file);
    assert!(
        !diags.iter().any(|d| d.code.as_ref() == "unresolved-method"
            || d.code.as_ref() == "unresolved-static-member"),
        "expected static-imported members to resolve, got {diags:?}"
    );
}

#[test]
fn static_single_import_resolves_workspace_members_across_files() {
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

    let src_a = r#"
 package p;
 class A {
   static int F = 1;
  static String m(int x) { return "x"; }
}
"#;
    let src_b = r#"
package p;
import static p.A.F;
import static p.A.m;
class B {
  void test() {
    int x = F;
    String s = m(1);
  }
}
"#;

    set_file(&mut db, project, a_file, "src/p/A.java", src_a);
    set_file(&mut db, project, b_file, "src/p/B.java", src_b);
    db.set_project_files(project, Arc::new(vec![a_file, b_file]));

    let offset_f = src_b
        .find("int x = F")
        .expect("snippet should contain `int x = F`")
        + "int x = ".len();
    let f_ty = db
        .type_at_offset_display(b_file, offset_f as u32)
        .expect("expected a type at offset for F");
    assert_eq!(f_ty, "int");

    let offset_m = src_b.find("m(1)").expect("snippet should contain m(1)") + "m".len();
    let m_ty = db
        .type_at_offset_display(b_file, offset_m as u32)
        .expect("expected a type at offset for m(1)");
    assert_eq!(m_ty, "String");

    let diags = db.type_diagnostics(b_file);
    assert!(
        !diags.iter().any(|d| {
            (d.code.as_ref() == "unresolved-name" || d.code.as_ref() == "unresolved-method")
                && (d.message.contains("`F`") || d.message.contains("`m`"))
        }),
        "expected static imports to resolve without unresolved-name/unresolved-method; got {diags:?}"
    );
}

#[test]
fn static_single_import_allows_field_and_method_with_same_name_across_files() {
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

    let src_a = r#"
package p;
class A {
  static int foo = 1;
  static String foo(int x) { return "x"; }
}
"#;
    let src_b = r#"
package p;
import static p.A.foo;
class B {
  void test() {
    int x = foo;
    String s = foo(1);
  }
}
"#;

    set_file(&mut db, project, a_file, "src/p/A.java", src_a);
    set_file(&mut db, project, b_file, "src/p/B.java", src_b);
    db.set_project_files(project, Arc::new(vec![a_file, b_file]));

    let offset_field = src_b
        .find("int x = foo")
        .expect("snippet should contain `int x = foo`")
        + "int x = ".len();
    let field_ty = db
        .type_at_offset_display(b_file, offset_field as u32)
        .expect("expected a type at offset for foo field");
    assert_eq!(field_ty, "int");

    let offset_call = src_b.find("foo(1)").expect("snippet should contain foo(1)") + "foo".len();
    let call_ty = db
        .type_at_offset_display(b_file, offset_call as u32)
        .expect("expected a type at offset for foo(1)");
    assert_eq!(call_ty, "String");

    let diags = db.type_diagnostics(b_file);
    assert!(
        !diags.iter().any(|d| {
            (d.code.as_ref() == "unresolved-name" || d.code.as_ref() == "unresolved-method")
                && d.message.contains("`foo`")
        }),
        "expected static import to allow both field + method references; got {diags:?}"
    );
}

#[test]
fn static_import_field_name_does_not_block_explicit_generic_invocation() {
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

    let src_a = r#"
package p;
class A {
  static int foo = 1;
  static <T> T foo() { return null; }
}
"#;
    let src_b = r#"
package p;
import static p.A.foo;
class B {
  void test() {
    var s = <String>foo();
    s.substring(1);
  }
}
"#;

    set_file(&mut db, project, a_file, "src/p/A.java", src_a);
    set_file(&mut db, project, b_file, "src/p/B.java", src_b);
    db.set_project_files(project, Arc::new(vec![a_file, b_file]));

    let diags = db.type_diagnostics(b_file);

    let offset = src_b
        .find("foo()")
        .expect("snippet should contain foo()")
        + "foo".len();
    let ty = db
        .type_at_offset_display(b_file, offset as u32)
        .expect("expected a type at offset for foo()");
    assert_eq!(ty, "String", "expected foo() to return String, got {ty:?} diags={diags:?}");
    assert!(
        diags
            .iter()
            .all(|d| !(d.code.as_ref() == "unresolved-method" && d.message.contains("substring"))),
        "expected explicit type args to make foo() return String, got {diags:?}"
    );
}

#[test]
fn static_import_resolves_enum_constants_across_files() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let enum_file = FileId::from_raw(1);
    let use_file = FileId::from_raw(2);

    let src_enum = r#"package p; enum E { A; }"#;
    let src_use = r#"
package p;
import static p.E.A;
class Use { static E m(){ return A; } }
"#;

    set_file(&mut db, project, enum_file, "src/p/E.java", src_enum);
    set_file(&mut db, project, use_file, "src/p/Use.java", src_use);
    db.set_project_files(project, Arc::new(vec![enum_file, use_file]));

    let offset = src_use
        .find("return A")
        .expect("snippet should contain return A")
        + "return ".len();
    let ty = db
        .type_at_offset_display(use_file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "E");

    let diags = db.type_diagnostics(use_file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-name"),
        "expected static-imported enum constant to resolve; got {diags:?}"
    );
}

#[test]
fn static_import_resolves_interface_fields_across_files() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let iface_file = FileId::from_raw(1);
    let use_file = FileId::from_raw(2);

    let src_iface = r#"package p; interface I { int X = 1; }"#;
    let src_use = r#"
package p;
import static p.I.X;
class Use { static int m(){ return X; } }
"#;

    set_file(&mut db, project, iface_file, "src/p/I.java", src_iface);
    set_file(&mut db, project, use_file, "src/p/Use.java", src_use);
    db.set_project_files(project, Arc::new(vec![iface_file, use_file]));

    let offset = src_use
        .find("return X")
        .expect("snippet should contain return X")
        + "return ".len();
    let ty = db
        .type_at_offset_display(use_file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "int");

    let diags = db.type_diagnostics(use_file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-name"),
        "expected static-imported interface field to resolve; got {diags:?}"
    );
}

#[test]
fn static_import_resolves_annotation_constants_across_files() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let ann_file = FileId::from_raw(1);
    let use_file = FileId::from_raw(2);

    let src_ann = r#"package p; @interface A { int X = 1; }"#;
    let src_use = r#"
package p;
import static p.A.X;
class Use { static int m(){ return X; } }
"#;

    set_file(&mut db, project, ann_file, "src/p/A.java", src_ann);
    set_file(&mut db, project, use_file, "src/p/Use.java", src_use);
    db.set_project_files(project, Arc::new(vec![ann_file, use_file]));

    let offset = src_use
        .find("return X")
        .expect("snippet should contain return X")
        + "return ".len();
    let ty = db
        .type_at_offset_display(use_file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "int");

    let diags = db.type_diagnostics(use_file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-name"),
        "expected static-imported annotation constant to resolve; got {diags:?}"
    );
}

#[test]
fn static_single_import_resolves_workspace_member_type_across_files() {
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

    let src_a = r#"
package p;
class A {
  static class Inner {}
}
"#;
    let src_b = r#"
package p;
import static p.A.Inner;
class B {
  void test() {
    Inner x = new Inner();
  }
}
"#;

    set_file(&mut db, project, a_file, "src/p/A.java", src_a);
    set_file(&mut db, project, b_file, "src/p/B.java", src_b);
    db.set_project_files(project, Arc::new(vec![a_file, b_file]));

    let offset = src_b
        .find("Inner()")
        .expect("snippet should contain Inner()")
        + "Inner".len();
    let ty = db
        .type_at_offset_display(b_file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "A.Inner");

    let diags = db.type_diagnostics(b_file);
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_ref() == "unresolved-type" && d.message.contains("Inner")),
        "expected static-imported member type `Inner` to resolve, got {diags:?}"
    );
}

#[test]
fn static_star_import_resolves_workspace_member_type_across_files() {
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

    let src_a = r#"
package p;
class A {
  static class Inner {}
}
"#;
    let src_b = r#"
package p;
import static p.A.*;
class B {
  void test() {
    Inner x = new Inner();
  }
}
"#;

    set_file(&mut db, project, a_file, "src/p/A.java", src_a);
    set_file(&mut db, project, b_file, "src/p/B.java", src_b);
    db.set_project_files(project, Arc::new(vec![a_file, b_file]));

    let offset = src_b
        .find("Inner()")
        .expect("snippet should contain Inner()")
        + "Inner".len();
    let ty = db
        .type_at_offset_display(b_file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "A.Inner");

    let diags = db.type_diagnostics(b_file);
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_ref() == "unresolved-type" && d.message.contains("Inner")),
        "expected static-star-imported member type `Inner` to resolve, got {diags:?}"
    );
}

#[test]
fn static_imported_workspace_generic_method_infers_return_type() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let util_file = FileId::from_raw(1);
    let use_file = FileId::from_raw(2);

    let src_util = r#"
package p;
class Util {
  static <T> T make() { return null; }
}
"#;
    let src_use = r#"
package p;
import static p.Util.make;
class Use {
  String m() { return make(); }
}
"#;

    set_file(&mut db, project, util_file, "src/p/Util.java", src_util);
    set_file(&mut db, project, use_file, "src/p/Use.java", src_use);
    db.set_project_files(project, Arc::new(vec![util_file, use_file]));

    let diags = db.type_diagnostics(use_file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected static-imported workspace generic method to resolve; got {diags:?}"
    );

    let offset = src_use
        .find("make()")
        .expect("snippet should contain make()")
        + "make".len();
    let ty = db
        .type_at_offset_display(use_file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "String");
}

#[test]
fn static_single_import_resolves_workspace_interface_field_across_files() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let i_file = FileId::from_raw(1);
    let use_file = FileId::from_raw(2);

    let src_i = r#"
package p;
interface I { int X = 1; }
"#;
    let src_use = r#"
package p;
import static p.I.X;
class Use {
  int m() { return X; }
}
"#;

    set_file(&mut db, project, i_file, "src/p/I.java", src_i);
    set_file(&mut db, project, use_file, "src/p/Use.java", src_use);
    db.set_project_files(project, Arc::new(vec![i_file, use_file]));

    let diags = db.type_diagnostics(use_file);
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_ref() == "unresolved-name" && d.message.contains("`X`")),
        "expected static-imported interface field to resolve; got {diags:?}"
    );

    let offset = src_use
        .find("return X")
        .expect("snippet should contain return X")
        + "return ".len();
    let ty = db
        .type_at_offset_display(use_file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "int");
}

#[test]
fn static_single_import_resolves_workspace_enum_constant_across_files() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let e_file = FileId::from_raw(1);
    let use_file = FileId::from_raw(2);

    let src_e = r#"
package p;
enum E { A; }
"#;
    let src_use = r#"
package p;
import static p.E.A;
class Use {
  E m() { return A; }
}
"#;

    set_file(&mut db, project, e_file, "src/p/E.java", src_e);
    set_file(&mut db, project, use_file, "src/p/Use.java", src_use);
    db.set_project_files(project, Arc::new(vec![e_file, use_file]));

    let diags = db.type_diagnostics(use_file);
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_ref() == "unresolved-name" && d.message.contains("`A`")),
        "expected static-imported enum constant to resolve; got {diags:?}"
    );

    let offset = src_use
        .find("return A")
        .expect("snippet should contain return A")
        + "return ".len();
    let ty = db
        .type_at_offset_display(use_file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "E");
}

#[test]
fn static_imported_math_max_resolves() {
    let src = r#"
import static java.lang.Math.max;
class C { int m(){ return max(1,2); } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected static-imported Math.max call to resolve; got {diags:?}"
    );
}

#[test]
fn static_imported_math_min_resolves() {
    let src = r#"
import static java.lang.Math.min;
class C { int m(){ return min(1,2); } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected static-imported Math.min call to resolve; got {diags:?}"
    );
}

#[test]
fn static_imported_math_max_long_resolves() {
    let src = r#"
import static java.lang.Math.max;
class C {
    long m() {
        long a = 1;
        long b = 2;
        return max(a, b);
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected static-imported Math.max(long,long) call to resolve; got {diags:?}"
    );
}

#[test]
fn static_imported_math_max_float_resolves() {
    let src = r#"
import static java.lang.Math.max;
class C {
    float m() {
        float a = 1;
        float b = 2;
        return max(a, b);
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected static-imported Math.max(float,float) call to resolve; got {diags:?}"
    );
}

#[test]
fn static_star_imported_math_max_resolves() {
    let src = r#"
import static java.lang.Math.*;
class C { int m(){ return max(1,2); } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"
            && d.code.as_ref() != "unresolved-name"
            && d.code.as_ref() != "unresolved-static-member"),
        "expected Math.* static star import to provide max; got {diags:?}"
    );
}

#[test]
fn static_star_imported_math_min_resolves() {
    let src = r#"
import static java.lang.Math.*;
class C { int m(){ return min(1,2); } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"
            && d.code.as_ref() != "unresolved-name"
            && d.code.as_ref() != "unresolved-static-member"),
        "expected Math.* static star import to provide min; got {diags:?}"
    );
}

#[test]
fn java_lang_math_max_resolves_via_implicit_import() {
    let src = r#"
class C {
    int m() {
        return Math.max(1, 2);
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected Math.max(int,int) to resolve via implicit java.lang import; got {diags:?}"
    );

    let offset = src.find("max(").expect("snippet should contain max call") + "max".len();
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "int");
}

#[test]
fn java_lang_math_min_resolves_via_implicit_import() {
    let src = r#"
class C {
    int m() {
        return Math.min(1, 2);
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected Math.min(int,int) to resolve via implicit java.lang import; got {diags:?}"
    );

    let offset = src.find("min(").expect("snippet should contain min call") + "min".len();
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "int");
}

#[test]
fn static_imported_math_pi_resolves() {
    let src = r#"
import static java.lang.Math.PI;
class C { double m(){ return PI; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.code.as_ref() != "unresolved-static-member"
                && d.code.as_ref() != "unresolved-field"),
        "expected static-imported Math.PI to resolve; got {diags:?}"
    );

    let offset = src
        .find("return PI")
        .expect("snippet should contain PI return")
        + "return ".len();
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "double");
}

#[test]
fn static_imported_math_e_resolves() {
    let src = r#"
import static java.lang.Math.E;
class C { double m(){ return E; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.code.as_ref() != "unresolved-static-member"
                && d.code.as_ref() != "unresolved-field"),
        "expected static-imported Math.E to resolve; got {diags:?}"
    );

    let offset = src
        .find("return E")
        .expect("snippet should contain E return")
        + "return ".len();
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "double");
}

#[test]
fn static_star_imported_math_pi_resolves() {
    let src = r#"
import static java.lang.Math.*;
class C { double m(){ return PI; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.code.as_ref() != "unresolved-static-member"
                && d.code.as_ref() != "unresolved-field"
                && d.code.as_ref() != "unresolved-name"),
        "expected Math.* static star import to provide PI; got {diags:?}"
    );

    let offset = src
        .find("return PI")
        .expect("snippet should contain PI return")
        + "return ".len();
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "double");
}

#[test]
fn static_star_imported_math_e_resolves() {
    let src = r#"
import static java.lang.Math.*;
class C { double m(){ return E; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.code.as_ref() != "unresolved-static-member"
                && d.code.as_ref() != "unresolved-field"
                && d.code.as_ref() != "unresolved-name"),
        "expected Math.* static star import to provide E; got {diags:?}"
    );

    let offset = src
        .find("return E")
        .expect("snippet should contain E return")
        + "return ".len();
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "double");
}

#[test]
fn java_lang_math_pi_resolves_via_implicit_import() {
    let src = r#"
class C {
    double m() {
        return Math.PI;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-field"
            && d.code.as_ref() != "unresolved-static-member"),
        "expected Math.PI to resolve via implicit java.lang import; got {diags:?}"
    );

    let offset = src.find("PI").expect("snippet should contain PI") + 1;
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "double");
}

#[test]
fn java_lang_math_e_resolves_via_implicit_import() {
    let src = r#"
class C {
    double m() {
        return Math.E;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-field"
            && d.code.as_ref() != "unresolved-static-member"),
        "expected Math.E to resolve via implicit java.lang import; got {diags:?}"
    );

    let offset = src.find("Math.E").expect("snippet should contain Math.E") + "Math.".len();
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "double");
}

#[test]
fn cross_file_type_reference_resolves_via_import_in_signature() {
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
        "package p; public class A {}",
    );
    set_file(
        &mut db,
        project,
        b_file,
        "src/q/B.java",
        "package q; import p.A; class B { A m() { return null; } }",
    );
    db.set_project_files(project, Arc::new(vec![a_file, b_file]));

    let diags = db.type_diagnostics(b_file);
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_ref() == "unresolved-type" && d.message.contains("`A`")),
        "expected `A` to resolve via workspace import during typeck, got {diags:?}"
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

    set_file(
        &mut db,
        project,
        a_file,
        "src/p/Foo.java",
        "package p; class Foo {}",
    );
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
fn cross_file_signature_return_type_resolves_in_same_package() {
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
        "package p; class A {}",
    );
    set_file(
        &mut db,
        project,
        b_file,
        "src/p/B.java",
        "package p; class B { A m() { return null; } }",
    );
    db.set_project_files(project, Arc::new(vec![a_file, b_file]));

    let diags = db.type_diagnostics(b_file);
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_ref() == "unresolved-type" && d.message.contains("`A`")),
        "expected return type `A` to resolve via workspace def map, got {diags:?}"
    );
}

#[test]
fn cross_file_signature_type_resolves_via_import() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let foo_file = FileId::from_raw(1);
    let bar_file = FileId::from_raw(2);

    set_file(
        &mut db,
        project,
        foo_file,
        "src/p/Foo.java",
        "package p; class Foo {}",
    );
    set_file(
        &mut db,
        project,
        bar_file,
        "src/q/Bar.java",
        "package q; import p.Foo; class Bar { Foo id(Foo x) { return x; } }",
    );
    db.set_project_files(project, Arc::new(vec![foo_file, bar_file]));

    let diags = db.type_diagnostics(bar_file);
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_ref() == "unresolved-type" && d.message.contains("Foo")),
        "expected `Foo` to resolve via workspace import, got {diags:?}"
    );
}

#[test]
fn cross_file_signature_type_resolves_via_star_import() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let foo_file = FileId::from_raw(1);
    let bar_file = FileId::from_raw(2);

    set_file(
        &mut db,
        project,
        foo_file,
        "src/p/Foo.java",
        "package p; class Foo {}",
    );
    set_file(
        &mut db,
        project,
        bar_file,
        "src/q/Bar.java",
        "package q; import p.*; class Bar { Foo id(Foo x) { return x; } }",
    );
    db.set_project_files(project, Arc::new(vec![foo_file, bar_file]));

    let diags = db.type_diagnostics(bar_file);
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_ref() == "unresolved-type" && d.message.contains("Foo")),
        "expected `Foo` to resolve via workspace star import, got {diags:?}"
    );
}

#[test]
fn cross_file_nested_type_reference_resolves_in_same_package() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let outer_file = FileId::from_raw(1);
    let use_file = FileId::from_raw(2);

    set_file(
        &mut db,
        project,
        outer_file,
        "src/p/Outer.java",
        "package p; class Outer { static class Inner {} }",
    );
    set_file(
        &mut db,
        project,
        use_file,
        "src/p/Use.java",
        "package p; class Use { Outer.Inner x; }",
    );
    db.set_project_files(project, Arc::new(vec![outer_file, use_file]));

    let diags = db.type_diagnostics(use_file);
    assert!(
        !diags.iter().any(|d| d.code.as_ref() == "unresolved-type"),
        "expected nested workspace type to resolve via qualified name, got {diags:?}"
    );
}

#[test]
fn cross_file_nested_type_resolves_via_import() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let outer_file = FileId::from_raw(1);
    let use_file = FileId::from_raw(2);

    set_file(
        &mut db,
        project,
        outer_file,
        "src/p/Outer.java",
        "package p; class Outer { static class Inner {} }",
    );
    set_file(
        &mut db,
        project,
        use_file,
        "src/q/Use.java",
        "package q; import p.Outer.Inner; class Use { Inner x; }",
    );
    db.set_project_files(project, Arc::new(vec![outer_file, use_file]));

    let diags = db.type_diagnostics(use_file);
    assert!(
        !diags.iter().any(|d| d.code.as_ref() == "unresolved-type"),
        "expected nested workspace type to resolve via single-type import, got {diags:?}"
    );
}

#[test]
fn cross_file_nested_type_resolves_via_type_star_import() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let outer_file = FileId::from_raw(1);
    let use_file = FileId::from_raw(2);

    set_file(
        &mut db,
        project,
        outer_file,
        "src/p/Outer.java",
        "package p; class Outer { static class Inner {} }",
    );
    set_file(
        &mut db,
        project,
        use_file,
        "src/q/Use.java",
        "package q; import p.Outer.*; class Use { Inner x; }",
    );
    db.set_project_files(project, Arc::new(vec![outer_file, use_file]));

    let diags = db.type_diagnostics(use_file);
    assert!(
        !diags.iter().any(|d| d.code.as_ref() == "unresolved-type"),
        "expected nested workspace type to resolve via type-import-on-demand, got {diags:?}"
    );
}

#[test]
fn cross_file_nested_type_reference_resolves_via_imported_outer() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let outer_file = FileId::from_raw(1);
    let use_file = FileId::from_raw(2);

    set_file(
        &mut db,
        project,
        outer_file,
        "src/p/Outer.java",
        "package p; class Outer { static class Inner {} }",
    );
    set_file(
        &mut db,
        project,
        use_file,
        "src/q/Use.java",
        "package q; import p.Outer; class Use { Outer.Inner x; }",
    );
    db.set_project_files(project, Arc::new(vec![outer_file, use_file]));

    let diags = db.type_diagnostics(use_file);
    assert!(
        !diags.iter().any(|d| d.code.as_ref() == "unresolved-type"),
        "expected Outer.Inner to resolve after importing Outer, got {diags:?}"
    );
}

#[test]
fn cross_file_nested_type_reference_resolves_via_package_star_import() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let outer_file = FileId::from_raw(1);
    let use_file = FileId::from_raw(2);

    set_file(
        &mut db,
        project,
        outer_file,
        "src/p/Outer.java",
        "package p; class Outer { static class Inner {} }",
    );
    set_file(
        &mut db,
        project,
        use_file,
        "src/q/Use.java",
        "package q; import p.*; class Use { Outer.Inner x; }",
    );
    db.set_project_files(project, Arc::new(vec![outer_file, use_file]));

    let diags = db.type_diagnostics(use_file);
    assert!(
        !diags.iter().any(|d| d.code.as_ref() == "unresolved-type"),
        "expected Outer.Inner to resolve via package star import, got {diags:?}"
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
fn workspace_type_wins_over_classpath_when_binary_names_collide() {
    // Ensure the classpath contains com.example.dep.Foo.
    let classpath = ClasspathIndex::build_with_deps_store(
        &[ClasspathEntry::Jar(test_dep_jar())],
        None,
        None,
        None,
    )
    .expect("failed to build dep.jar classpath index");

    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, Some(ArcEq::new(Arc::new(classpath))));

    let foo_file = FileId::from_raw(1);
    let use_file = FileId::from_raw(2);

    // Workspace also defines com.example.dep.Foo; this should win over the classpath/JDK.
    set_file(
        &mut db,
        project,
        foo_file,
        "src/com/example/dep/Foo.java",
        r#"package com.example.dep; class Foo { void foo() {} }"#,
    );
    let src_use = r#"package com.example.dep; class Use { void test() { new Foo().foo(); } }"#;
    set_file(
        &mut db,
        project,
        use_file,
        "src/com/example/dep/Use.java",
        src_use,
    );
    db.set_project_files(project, Arc::new(vec![foo_file, use_file]));

    let diags = db.type_diagnostics(use_file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected method call to resolve against workspace Foo; got {diags:?}"
    );

    let tree = db.hir_item_tree(use_file);
    let test_method = find_method_named(&tree, "test");
    let body = db.typeck_body(DefWithBodyId::Method(test_method));

    let foo_id = body
        .env
        .lookup_class("com.example.dep.Foo")
        .expect("expected Foo to be present in body env");
    let foo_def = body.env.class(foo_id).expect("expected Foo ClassDef");
    assert!(
        foo_def.methods.iter().any(|m| m.name == "foo"),
        "expected Foo in env to come from workspace source (contain foo()); got methods={:?}",
        foo_def
            .methods
            .iter()
            .map(|m| m.name.as_str())
            .collect::<Vec<_>>()
    );
}

#[test]
fn workspace_supertype_is_not_overwritten_when_loading_external_subclass() {
    // dep.jar contains:
    // - com.example.dep.Foo (no `foo()` method)
    // - com.example.dep.Bar extends Foo
    //
    // This test ensures that when we load the external `Bar` stub, the external loader's
    // recursive `ensure_class(Foo)` does not overwrite the workspace `Foo` definition.

    let classpath = ClasspathIndex::build_with_deps_store(
        &[ClasspathEntry::Jar(test_dep_jar())],
        None,
        None,
        None,
    )
    .expect("failed to build dep.jar classpath index");

    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, Some(ArcEq::new(Arc::new(classpath))));

    let foo_file = FileId::from_raw(1);
    let use_file = FileId::from_raw(2);

    // Workspace defines Foo with `foo()`, but Bar comes from the classpath.
    set_file(
        &mut db,
        project,
        foo_file,
        "src/com/example/dep/Foo.java",
        r#"package com.example.dep; class Foo { void foo() {} }"#,
    );
    set_file(
        &mut db,
        project,
        use_file,
        "src/com/example/dep/Use.java",
        r#"
package com.example.dep;
class Use {
    void test() {
        new Bar().foo();
    }
}
"#,
    );
    db.set_project_files(project, Arc::new(vec![foo_file, use_file]));

    let diags = db.type_diagnostics(use_file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected Bar to inherit foo() from workspace Foo; got {diags:?}"
    );

    let tree = db.hir_item_tree(use_file);
    let test_method = find_method_named(&tree, "test");
    let body = db.typeck_body(DefWithBodyId::Method(test_method));

    let foo_id = body
        .env
        .lookup_class("com.example.dep.Foo")
        .expect("expected Foo to be present in body env");
    let foo_def = body.env.class(foo_id).expect("expected Foo ClassDef");
    assert!(
        foo_def.methods.iter().any(|m| m.name == "foo"),
        "expected Foo in env to come from workspace source (contain foo()); got methods={:?}",
        foo_def
            .methods
            .iter()
            .map(|m| m.name.as_str())
            .collect::<Vec<_>>()
    );

    let bar_id = body
        .env
        .lookup_class("com.example.dep.Bar")
        .expect("expected Bar to be loaded from classpath");
    let bar_def = body.env.class(bar_id).expect("expected Bar ClassDef");
    let super_ty = bar_def
        .super_class
        .as_ref()
        .expect("expected Bar to have a super class");
    match super_ty {
        Type::Class(ty) => assert_eq!(
            ty.def, foo_id,
            "expected Bar super class to resolve to workspace Foo"
        ),
        other => panic!("expected Bar super type to be a Class, got {other:?}"),
    }
}

#[test]
fn workspace_type_name_expr_does_not_load_classpath_stub_with_same_binary_name() {
    // Ensure the classpath contains com.example.dep.Foo.
    let classpath = ClasspathIndex::build_with_deps_store(
        &[ClasspathEntry::Jar(test_dep_jar())],
        None,
        None,
        None,
    )
    .expect("failed to build dep.jar classpath index");

    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, Some(ArcEq::new(Arc::new(classpath))));

    let foo_file = FileId::from_raw(1);
    let use_file = FileId::from_raw(2);

    // dep.jar's `com.example.dep.Foo` stub does *not* include `static foo()`. Ensure we resolve the
    // call through the workspace definition (no collisions / accidental overwrites from
    // `ExternalTypeLoader`).
    set_file(
        &mut db,
        project,
        foo_file,
        "src/com/example/dep/Foo.java",
        r#"
package com.example.dep;
class Foo {
    static void foo() {}
}
"#,
    );
    set_file(
        &mut db,
        project,
        use_file,
        "src/com/example/dep/Use.java",
        r#"
package com.example.dep;
class Use {
    void m() {
        Foo.foo();
    }
}
"#,
    );
    db.set_project_files(project, Arc::new(vec![foo_file, use_file]));

    let diags = db.type_diagnostics(use_file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"
            && d.code.as_ref() != "unresolved-static-member"),
        "expected Foo.foo() to resolve against workspace Foo (not dep.jar), got {diags:?}"
    );
}

#[test]
fn qualified_type_name_expr_does_not_overwrite_workspace_class_def() {
    // Ensure the classpath contains com.example.dep.Foo.
    let classpath = ClasspathIndex::build_with_deps_store(
        &[ClasspathEntry::Jar(test_dep_jar())],
        None,
        None,
        None,
    )
    .expect("failed to build dep.jar classpath index");

    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, Some(ArcEq::new(Arc::new(classpath))));

    let foo_file = FileId::from_raw(1);
    let use_file = FileId::from_raw(2);

    set_file(
        &mut db,
        project,
        foo_file,
        "src/com/example/dep/Foo.java",
        r#"
package com.example.dep;
class Foo {
    static void workspaceMarker() {}
}
"#,
    );
    set_file(
        &mut db,
        project,
        use_file,
        "src/Use.java",
        r#"
class Use {
    void m() {
        // This is not a valid statement expression, but we still want type-name expressions to
        // avoid loading colliding classpath stubs (which can overwrite the workspace ClassDef).
        com.example.dep.Foo;
    }
}
"#,
    );
    db.set_project_files(project, Arc::new(vec![foo_file, use_file]));

    let tree = db.hir_item_tree(use_file);
    let m = find_method_named(&tree, "m");
    let body = db.typeck_body(DefWithBodyId::Method(m));

    let foo_id = body
        .env
        .lookup_class("com.example.dep.Foo")
        .expect("expected Foo to be present in body env");
    let foo_def = body.env.class(foo_id).expect("expected Foo ClassDef");
    assert!(
        foo_def.methods.iter().any(|m| m.name == "workspaceMarker"),
        "expected Foo in env to come from workspace source (contain workspaceMarker()); got methods={:?}",
        foo_def
            .methods
            .iter()
            .map(|m| m.name.as_str())
            .collect::<Vec<_>>()
    );
}

#[test]
fn workspace_fully_qualified_type_name_expr_does_not_load_classpath_stub_with_same_binary_name() {
    // Ensure the classpath contains com.example.dep.Foo.
    let classpath = ClasspathIndex::build_with_deps_store(
        &[ClasspathEntry::Jar(test_dep_jar())],
        None,
        None,
        None,
    )
    .expect("failed to build dep.jar classpath index");

    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, Some(ArcEq::new(Arc::new(classpath))));

    let foo_file = FileId::from_raw(1);
    let use_file = FileId::from_raw(2);

    // dep.jar's `com.example.dep.Foo` stub does *not* include `static foo()`. Ensure we resolve the
    // call through the workspace definition even when referenced via a fully-qualified name
    // expression (`com.example.dep.Foo.foo()`), which is lowered as a `FieldAccess` chain.
    set_file(
        &mut db,
        project,
        foo_file,
        "src/com/example/dep/Foo.java",
        r#"
package com.example.dep;
class Foo {
    static void foo() {}
}
"#,
    );
    set_file(
        &mut db,
        project,
        use_file,
        "src/com/example/other/Use.java",
        r#"
package com.example.other;
class Use {
    void m() {
        com.example.dep.Foo.foo();
    }
}
"#,
    );
    db.set_project_files(project, Arc::new(vec![foo_file, use_file]));

    let diags = db.type_diagnostics(use_file);
    assert!(
        diags.iter().all(|d| {
            d.code.as_ref() != "unresolved-method"
                && d.code.as_ref() != "unresolved-static-member"
                && d.code.as_ref() != "static-context"
        }),
        "expected com.example.dep.Foo.foo() to resolve against workspace Foo (not dep.jar), got {diags:?}"
    );

    // Ensure the body env's `Foo` def is still the workspace one (contains `static foo()`).
    let tree = db.hir_item_tree(use_file);
    let m = find_method_named(&tree, "m");
    let body = db.typeck_body(DefWithBodyId::Method(m));

    let foo_id = body
        .env
        .lookup_class("com.example.dep.Foo")
        .expect("expected Foo to be present in body env");
    let foo_def = body.env.class(foo_id).expect("expected Foo ClassDef");
    assert!(
        foo_def.methods.iter().any(|m| m.name == "foo" && m.is_static),
        "expected Foo in env to come from workspace source (contain static foo()); got methods={:?}",
        foo_def
            .methods
            .iter()
            .map(|m| {
                format!(
                    "{}{}",
                    if m.is_static { "static " } else { "" },
                    m.name.as_str()
                )
            })
            .collect::<Vec<_>>()
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
fn foreach_var_infers_array_element_type() {
    let src = r#"
class C {
    void m() {
        for (var s : new String[0]) {
            s.length();
        }
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| !(d.code.as_ref() == "unresolved-method" && d.message.contains("length"))),
        "expected foreach var element type to resolve String.length(), got {diags:?}"
    );

    let offset = src
        .find("s.length()")
        .expect("snippet should contain `s.length()`");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "String");
}

#[test]
fn var_is_not_inferred_below_java_10() {
    let src = r#"
class C {
    void m() {
        var x = 1;
        x.toString();
    }
}
"#;

    let (db, file) = setup_db_with_source(src, JavaVersion::JAVA_8);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "unresolved-type" && d.message.contains("var")),
        "expected `var` to be treated as an unresolved type below Java 10; got {diags:?}"
    );

    let offset = src
        .find("x.toString")
        .expect("snippet should contain `x.toString`");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "var");
}

#[test]
fn var_self_reference_does_not_panic() {
    let src = r#"
class C {
    void m() {
        var x = x;
    }
}
"#;

    let (db, file) = setup_db(src);
    let offset = src
        .find("= x")
        .expect("snippet should contain initializer x")
        + "= ".len();
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "<?>");
}

#[test]
fn foreach_var_is_not_inferred_below_java_10() {
    let src = r#"
class C {
    void m() {
        for (var s : new String[0]) {
            s.length();
        }
    }
}
"#;

    let (db, file) = setup_db_with_source(src, JavaVersion::JAVA_8);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "unresolved-type" && d.message.contains("var")),
        "expected `var` to be treated as an unresolved type below Java 10; got {diags:?}"
    );
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "unresolved-method" && d.message.contains("length")),
        "expected foreach var to not be inferred below Java 10; got {diags:?}"
    );

    let offset = src
        .find("s.length()")
        .expect("snippet should contain `s.length()`");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "var");
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
fn unresolved_abstract_method_signature_types_are_anchored() {
    let src = r#"
interface I {
    Missing m(AlsoMissing x);
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
            snippet == "Missing" || snippet == "AlsoMissing",
            "expected span to cover the unresolved type name, got {snippet:?} for {span:?}"
        );
    }
}

#[test]
fn unresolved_field_types_are_anchored() {
    let src = r#"
class C {
    Missing field;
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    let diag = diags
        .iter()
        .find(|d| d.code.as_ref() == "unresolved-type" && d.message.contains("Missing"))
        .expect("expected unresolved-type diagnostic for field type");
    let span = diag
        .span
        .expect("unresolved-type diagnostic should have a span");
    assert_eq!(&src[span.start..span.end], "Missing");
}

#[test]
fn unresolved_extends_clause_types_are_anchored() {
    let src = r#"
class C extends Missing {
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    let diag = diags
        .iter()
        .find(|d| d.code.as_ref() == "unresolved-type" && d.message.contains("Missing"))
        .expect("expected unresolved-type diagnostic for extends clause");
    let span = diag
        .span
        .expect("unresolved-type diagnostic should have a span");
    assert_eq!(&src[span.start..span.end], "Missing");
}

#[test]
fn unresolved_annotation_types_are_anchored() {
    let src = r#"
@Missing
class C {
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    let diag = diags
        .iter()
        .find(|d| d.code.as_ref() == "unresolved-type" && d.message.contains("Missing"))
        .expect("expected unresolved-type diagnostic for annotation type");
    let span = diag
        .span
        .expect("unresolved-type diagnostic should have a span");
    assert_eq!(&src[span.start..span.end], "Missing");
}

#[test]
fn type_use_annotation_types_are_ignored_even_when_anchored() {
    let src = r#"
import java.util.List;

class C {
    List<@Missing String> xs;
    List<@ Missing String> ys;
    List<@/*comment*/Missing String> zs;
    List<@ // comment
        Missing String> ws;
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_ref() == "unresolved-type" && d.message.contains("Missing")),
        "expected type-use annotation types to be ignored; got {diags:?}"
    );
}

#[test]
fn type_use_annotation_types_are_ignored_with_whitespace_after_at() {
    let src = r#"
import java.util.List;

class C {
    List<@ Missing String> xs;
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_ref() == "unresolved-type" && d.message.contains("Missing")),
        "expected type-use annotation types to be ignored even with whitespace after `@`; got {diags:?}"
    );
}

#[test]
fn type_use_annotation_types_are_ignored_before_array_suffix() {
    let src = r#"
class C {
    String @Missing [] xs;
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_ref() == "unresolved-type" && d.message.contains("Missing")),
        "expected type-use annotation types to be ignored before array suffixes; got {diags:?}"
    );
}

#[test]
fn type_use_annotation_types_are_ignored_with_block_comment_after_at() {
    let src = r#"
import java.util.List;

class C {
    List<@/*comment*/Missing String> xs;
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_ref() == "unresolved-type" && d.message.contains("Missing")),
        "expected type-use annotation types to be ignored even with a comment after `@`; got {diags:?}"
    );
}

#[test]
fn type_use_annotation_types_are_ignored_with_line_comment_after_at() {
    let src = r#"
import java.util.List;

class C {
    List<@
        // comment
        Missing String> xs;
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_ref() == "unresolved-type" && d.message.contains("Missing")),
        "expected type-use annotation types to be ignored even with a line comment after `@`; got {diags:?}"
    );
}

#[test]
fn unresolved_class_type_param_bounds_are_anchored() {
    let src = r#"
class C<T extends Missing> {
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    let diag = diags
        .iter()
        .find(|d| d.code.as_ref() == "unresolved-type" && d.message.contains("Missing"))
        .expect("expected unresolved-type diagnostic for class type param bound");
    let span = diag
        .span
        .expect("unresolved-type diagnostic should have a span");
    assert_eq!(&src[span.start..span.end], "Missing");
}

#[test]
fn unresolved_type_param_bounds_are_anchored() {
    let src = r#"
class C {
    <T extends Missing> T id(T t) { return t; }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    let unresolved: Vec<_> = diags
        .iter()
        .filter(|d| d.code.as_ref() == "unresolved-type")
        .collect();
    assert!(
        unresolved.iter().any(|d| d.message.contains("Missing")),
        "expected an unresolved-type diagnostic for the type parameter bound; got {diags:?}"
    );

    for diag in unresolved {
        if !diag.message.contains("Missing") {
            continue;
        }
        let span = diag
            .span
            .expect("unresolved-type diagnostic should have a span");
        let snippet = &src[span.start..span.end];
        assert_eq!(
            snippet, "Missing",
            "expected span to cover the unresolved bound type name"
        );
    }
}

#[test]
fn unresolved_throws_clause_types_are_anchored() {
    let src = r#"
class C {
    void m() throws Missing, AlsoMissing { }
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
            snippet == "Missing" || snippet == "AlsoMissing",
            "expected span to cover the unresolved throws type name, got {snippet:?} for {span:?}"
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
fn return_without_value_in_nonvoid_is_error() {
    let src = r#"
class C { int m(){ return; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "return-mismatch"),
        "expected return-mismatch diagnostic, got {diags:?}"
    );
}

#[test]
fn returning_value_from_void_method_does_not_target_type_expression() {
    let src = r#"
class C {
    static <T> T id(T t) { return t; }
    void m() {
        return id("x");
    }
}
"#;

    let (db, file) = setup_db(src);
    let offset = src
        .rfind("id(\"x\"")
        .expect("snippet should contain id call")
        + "id".len();
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "String");
}

#[test]
fn comparison_expression_has_boolean_type() {
    let src = r#"
class C { boolean m(){ return 1 < 2; } }
"#;

    let (db, file) = setup_db(src);
    let offset = src.find('<').expect("snippet should contain <");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "boolean");
}

#[test]
fn this_type_allows_member_calls() {
    let src = r#"
class C { void foo(){} void m(){ this.foo(); } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected `this` receiver call to resolve, got {diags:?}"
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
fn generic_method_type_param_bounds_allow_member_calls() {
    let src = r#"
class C {
    <T extends String> String m(T t) {
        return t.substring(1);
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected bounded type param receiver to resolve member calls; got {diags:?}"
    );

    let offset = src
        .find("substring")
        .expect("snippet should contain substring call")
        + "substring".len();
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "String");
}

#[test]
fn generic_constructor_type_param_bounds_allow_member_calls() {
    let src = r#"
class Foo {
    <T extends String> Foo(T t) {
        t.substring(1);
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected bounded ctor type param receiver to resolve member calls; got {diags:?}"
    );

    let offset = src
        .find("substring")
        .expect("snippet should contain substring call")
        + "substring".len();
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "String");
}

#[test]
fn intersection_type_param_bounds_allow_member_calls_from_any_bound() {
    let src = r#"
interface I { void a(); }
interface J { String b(); }
class C {
    <T extends I & J> String m(T t) {
        return t.b();
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected intersection-bounded receiver to resolve methods from any bound; got {diags:?}"
    );

    let offset = src.find("t.b()").expect("snippet should contain call") + "t.b".len();
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "String");
}

#[test]
fn intersection_bounds_choose_most_specific_return_type() {
    let src = r#"
interface I { Number m(); }
interface J { Integer m(); }
class C {
    <T extends I & J> Integer f(T t) {
        return t.m();
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected intersection-bounded receiver to resolve method; got {diags:?}"
    );
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "type-mismatch"),
        "expected covariant intersection return type to avoid mismatch; got {diags:?}"
    );

    let offset = src.find("t.m()").expect("snippet should contain call") + "t.m".len();
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "Integer");
}

#[test]
fn intersection_bounds_return_type_can_be_intersection() {
    let src = r#"
interface A { }
interface B { }
interface I { A m(); }
interface J { B m(); }
class C {
    <T extends I & J> void f(T t) {
        A a = t.m();
        B b = t.m();
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected intersection-bounded receiver to resolve method; got {diags:?}"
    );
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "type-mismatch"),
        "expected intersection return type to be assignable to both bounds; got {diags:?}"
    );

    let offset = src.find("t.m()").expect("snippet should contain call") + "t.m".len();
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "A & B");
}

#[test]
fn intersection_bounds_with_class_and_interface_allow_member_calls_from_interface_bound() {
    let src = r#"
class A { }
interface I { String b(); }
class C {
    <T extends A & I> String m(T t) {
        return t.b();
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected intersection-bounded receiver to resolve methods from interface bound; got {diags:?}"
    );

    let offset = src.find("t.b()").expect("snippet should contain call") + "t.b".len();
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "String");
}

#[test]
fn intersection_bounds_merge_generic_method_type_params_across_bounds() {
    let src = r#"
interface I { <U> U id(U u); }
interface J { <V> V id(V v); }
class C {
    <T extends I & J> String m(T t) {
        return t.id("x");
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected intersection-bounded receiver to resolve generic method; got {diags:?}"
    );
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "type-mismatch"),
        "expected inferred return type to be assignable; got {diags:?}"
    );

    let offset = src
        .find("t.id(\"x\")")
        .expect("snippet should contain call")
        + "t.id".len();
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "String");
}

#[test]
fn intersection_bounds_prefer_generic_method_over_non_generic_duplicate_signature() {
    // `Object id(Object)` and `<T> T id(T)` have the same erased signature (`Object id(Object)`).
    // For intersection bounds, we should prefer keeping the generic declaration so inference can
    // pick a precise return type.
    let src = r#"
interface A { Object id(Object o); }
interface B { <T> T id(T t); }
class C {
    <X extends A & B> String m(X x) {
        return x.id("x");
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected intersection-bounded receiver to resolve method; got {diags:?}"
    );
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "type-mismatch"),
        "expected generic method to infer String and avoid mismatch; got {diags:?}"
    );

    let offset = src
        .find("x.id(\"x\")")
        .expect("snippet should contain call")
        + "x.id".len();
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "String");
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
fn lambda_param_type_is_inferred_from_function_target_substring() {
    let src = r#"
import java.util.function.Function;
class C {
    void m() {
        Function<String, String> f = s -> s.substring(1);
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unresolved-method"),
        "expected lambda body method call to resolve after parameter inference, got {diags:?}"
    );
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "type-mismatch"),
        "expected lambda assignment to type-check, got {diags:?}"
    );
}

#[test]
fn lambda_param_type_is_inferred_from_assignment_target() {
    let src = r#"
import java.util.function.Function;
class C {
    void m() {
        Function<String, Integer> f;
        f = s -> s.length();
    }
}
"#;

    let (db, file) = setup_db(src);
    let offset = src
        .find("s.length")
        .expect("snippet should contain lambda parameter usage");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "String");
}

#[test]
fn lambda_param_type_is_inferred_from_return_target() {
    let src = r#"
import java.util.function.Function;
class C {
    Function<String, Integer> m() {
        return s -> s.length();
    }
}
"#;

    let (db, file) = setup_db(src);
    let offset = src
        .find("s.length")
        .expect("snippet should contain lambda parameter usage");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "String");
}

#[test]
fn lambda_param_type_is_inferred_from_call_argument_target() {
    let src = r#"
import java.util.function.Function;
class C {
    void takes(Function<String, Integer> f) { }
    void m() {
        takes(s -> s.length());
    }
}
"#;

    let (db, file) = setup_db(src);
    db.clear_query_stats();

    let offset = src
        .find("s.length")
        .expect("snippet should contain lambda parameter usage");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "String");

    let stats = db.query_stats();
    let typeck_body_activity = stats
        .by_query
        .get("typeck_body")
        .map(|s| (s.executions, s.validated_memoized))
        .unwrap_or((0, 0));
    assert_eq!(
        typeck_body_activity,
        (0, 0),
        "type_at_offset_display should not invoke full-body type checking for call-arg lambdas"
    );
}

#[test]
fn lambda_param_type_is_inferred_from_call_argument_target_in_if_condition() {
    let src = r#"
import java.util.function.Function;
class C {
    boolean takes(Function<String, Integer> f) { return true; }
    void m() {
        if (takes(s -> s.length())) {
        }
    }
}
"#;

    let (db, file) = setup_db(src);
    db.clear_query_stats();

    let offset = src
        .find("s.length")
        .expect("snippet should contain lambda parameter usage");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "String");

    let stats = db.query_stats();
    let typeck_body_activity = stats
        .by_query
        .get("typeck_body")
        .map(|s| (s.executions, s.validated_memoized))
        .unwrap_or((0, 0));
    assert_eq!(
        typeck_body_activity,
        (0, 0),
        "type_at_offset_display should not invoke full-body type checking for call-arg lambdas"
    );
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

#[test]
fn lambda_block_return_checked_against_sam_substring() {
    let src = r#"
import java.util.function.Function;
class C {
    void m() {
        Function<String, String> f = s -> { return s.substring(1); };
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

#[test]
fn lambda_returns_do_not_use_enclosing_method_return_type_substring() {
    let src = r#"
import java.util.function.Function;
class C {
    int m() {
        Function<String, String> f = s -> { return s.substring(1); };
        return 0;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "return-mismatch"),
        "expected lambda return checking to not use enclosing method return, got {diags:?}"
    );
}

#[test]
fn lambda_return_value_is_error_for_runnable_target() {
    let src = r#"
class C {
    void m() {
        Runnable r = () -> { return 1; };
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "return-mismatch"),
        "expected return-mismatch diagnostic for Runnable lambda with value return, got {diags:?}"
    );
}

#[test]
fn lambda_expr_body_must_be_void_compatible_for_runnable_target() {
    let src = r#"
class C {
    void m() {
        Runnable r = () -> 1;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "return-mismatch"),
        "expected return-mismatch diagnostic for Runnable lambda with expression body, got {diags:?}"
    );
}

#[test]
fn lambda_expr_body_statement_expression_is_allowed_for_runnable_target() {
    let src = r#"
class C {
    void m() {
        Runnable r = () -> "x".substring(1);
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "return-mismatch"),
        "expected Runnable lambda expression body to be allowed, got {diags:?}"
    );
}

#[test]
fn lambda_block_return_without_value_is_error_for_nonvoid_sam() {
    let src = r#"
import java.util.function.Function;
class C {
    void m() {
        Function<String, Integer> f = s -> { return; };
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "return-mismatch"),
        "expected return-mismatch diagnostic for non-void lambda return; got {diags:?}"
    );
}

#[test]
fn lambda_block_return_without_value_is_allowed_for_void_sam() {
    let src = r#"
class C {
    void m() {
        Runnable r = () -> { return; };
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "return-mismatch"),
        "expected no return-mismatch diagnostic for void lambda return; got {diags:?}"
    );
}

#[test]
fn lambda_expr_body_void_is_error_for_nonvoid_sam() {
    let src = r#"
import java.util.function.Function;
class C {
    void m() {
        Function<String, Integer> f = s -> System.out.println(s);
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "return-mismatch"),
        "expected return-mismatch diagnostic for void expression body; got {diags:?}"
    );
}

#[test]
fn type_of_def_is_signature_only_and_does_not_execute_typeck_body() {
    let src = r#"
class C {
    int m() { return 1; }
}
"#;

    let (db, file) = setup_db(src);
    let tree = db.hir_item_tree(file);

    let method_id = tree
        .items
        .iter()
        .find_map(|item| match *item {
            nova_hir::item_tree::Item::Class(class_id) => tree
                .class(class_id)
                .members
                .iter()
                .find_map(|member| match *member {
                    nova_hir::item_tree::Member::Method(m) => Some(m),
                    _ => None,
                }),
            _ => None,
        })
        .expect("expected to find method in item tree");

    // Reset query stats so the assertion below only reflects the `type_of_def` call.
    db.clear_query_stats();

    let ty = db.type_of_def(DefWithBodyId::Method(method_id));
    assert_eq!(ty, Type::Primitive(PrimitiveType::Int));

    let typeck_body_executions = db
        .query_stats()
        .by_query
        .get("typeck_body")
        .map(|s| s.executions)
        .unwrap_or(0);
    assert_eq!(
        typeck_body_executions, 0,
        "type_of_def should not execute typeck_body"
    );
}

#[test]
fn class_ids_are_stable_across_files_for_workspace_source_types() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();
    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let file_a = FileId::from_raw(1);
    let file_b = FileId::from_raw(2);

    set_file(
        &mut db,
        project,
        file_a,
        "src/p/Shared.java",
        r#"
package p;

class Shared {
    Shared id(Shared x) { return x; }
}
"#,
    );
    set_file(
        &mut db,
        project,
        file_b,
        "src/p/User.java",
        r#"
package p;

class User {
    Shared id(Shared x) { return x; }
}
"#,
    );
    db.set_project_files(project, Arc::new(vec![file_a, file_b]));

    let body_a = db.typeck_body(first_method_with_body(&db, file_a));
    let body_b = db.typeck_body(first_method_with_body(&db, file_b));

    let shared_a = body_a
        .env
        .lookup_class("p.Shared")
        .expect("expected p.Shared to be interned in env");
    let shared_b = body_b
        .env
        .lookup_class("p.Shared")
        .expect("expected p.Shared to be interned in env");
    assert_eq!(
        shared_a, shared_b,
        "expected stable ClassId for workspace type p.Shared"
    );
}

#[test]
fn constructor_call_resolves_for_source_type() {
    let src = r#"
class C { C(int x) {} }
class D { void m(){ new C(1); } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.code.as_ref() != "unresolved-constructor"),
        "expected constructor call to resolve; got {diags:?}"
    );
}

#[test]
fn constructor_call_resolves_for_generic_class_type_param() {
    let src = r#"
class C<T> { C(T x) {} }
class D { void m(){ new C<String>("x"); } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.code.as_ref() != "unresolved-constructor"),
        "expected generic constructor call to resolve; got {diags:?}"
    );
}

#[test]
fn record_implicit_canonical_constructor_resolves() {
    let src = r#"
record R(int x) { }
class C { void m(){ new R(1); } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.code.as_ref() != "unresolved-constructor"),
        "expected record canonical constructor call to resolve; got {diags:?}"
    );
}

#[test]
fn record_varargs_canonical_constructor_call_resolves() {
    let src = r#"
record R(int... xs) { }
class C {
    void m() {
        new R();
        new R(1);
        new R(1, 2, 3);
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.code.as_ref() != "unresolved-constructor"),
        "expected record varargs constructor call to resolve; got {diags:?}"
    );
}

#[test]
fn class_ids_are_stable_across_files_for_jdk_nested_types() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();
    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let file_a = FileId::from_raw(1);
    let file_b = FileId::from_raw(2);

    // `java.util.Map` is present in the built-in JDK name index but not in
    // `TypeStore::with_minimal_jdk`. This test ensures we still assign stable
    // `ClassId`s for it (and its nested `Entry` type) across bodies/files.
    set_file(
        &mut db,
        project,
        file_a,
        "src/A.java",
        r#"
class A {
    void entry(java.util.Map.Entry e) {}
    void map(java.util.Map m) {}
}
"#,
    );
    set_file(
        &mut db,
        project,
        file_b,
        "src/B.java",
        r#"
class B {
    void map(java.util.Map m) {}
}
"#,
    );
    db.set_project_files(project, Arc::new(vec![file_a, file_b]));

    let body_a = db.typeck_body(first_method_with_body(&db, file_a));
    let body_b = db.typeck_body(first_method_with_body(&db, file_b));

    let map_a = body_a
        .env
        .lookup_class("java.util.Map")
        .expect("expected java.util.Map to be interned in env");
    let map_b = body_b
        .env
        .lookup_class("java.util.Map")
        .expect("expected java.util.Map to be interned in env");
    assert_eq!(map_a, map_b, "expected stable ClassId for java.util.Map");

    let entry_a = body_a
        .env
        .lookup_class("java.util.Map$Entry")
        .expect("expected java.util.Map$Entry to be interned in env");
    let entry_b = body_b
        .env
        .lookup_class("java.util.Map$Entry")
        .expect("expected java.util.Map$Entry to be interned in env");
    assert_eq!(
        entry_a, entry_b,
        "expected stable ClassId for nested type java.util.Map$Entry"
    );
}

#[test]
fn constructor_call_mismatch_reports_diag() {
    let src = r#"
class C { C(int x) {} }
class D { void m(){ new C("x"); } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "unresolved-constructor"),
        "expected unresolved-constructor diagnostic; got {diags:?}"
    );
}

#[test]
fn jdk_constructor_call_mismatch_reports_diag() {
    let src = r#"
import java.util.*;
class C { void m(){ new ArrayList(1, 2); } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "unresolved-constructor"),
        "expected unresolved-constructor diagnostic; got {diags:?}"
    );
}

#[test]
fn source_default_constructor_is_available() {
    let src = r#"
class C { }
class D { void m(){ new C(); } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.code.as_ref() != "unresolved-constructor"),
        "expected implicit default constructor to resolve; got {diags:?}"
    );
}

#[test]
fn source_record_canonical_constructor_is_available() {
    let src = r#"
record R(int x) { }
class Use { void m(){ new R(1); } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.code.as_ref() != "unresolved-constructor"),
        "expected record canonical constructor to resolve; got {diags:?}"
    );
}

#[test]
fn record_constructor_arity_mismatch_reports_diag() {
    let src = r#"
record R(int x) { }
class Use { void m(){ new R(); } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "unresolved-constructor"),
        "expected unresolved-constructor diagnostic; got {diags:?}"
    );
}

#[test]
fn private_constructor_is_not_accessible() {
    let src = r#"
class C { private C(int x) {} }
class D { void m(){ new C(1); } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(
            |d| d.code.as_ref() == "unresolved-constructor" && d.message.contains("accessible")
        ),
        "expected private constructor call to be rejected; got {diags:?}"
    );
}

#[test]
fn varargs_constructor_call_resolves() {
    let src = r#"
class Foo { Foo(int... xs) {} }
class Use {
    void m() {
        new Foo();
        new Foo(1);
        new Foo(1, 2, 3);
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.code.as_ref() != "unresolved-constructor"),
        "expected varargs constructor calls to resolve; got {diags:?}"
    );
}

#[test]
fn resolve_method_call_demand_resolves_constructor_call() {
    let src = r#"
class C { C(int x) {} }
class D { void m(){ new C(1); } }
"#;

    let (db, file) = setup_db(src);

    // Find the `new C(1)` expression inside `D.m`.
    let tree = db.hir_item_tree(file);
    let m_id = find_method_named(&tree, "m");
    let body = db.hir_body(m_id);
    let new_expr = body
        .stmts
        .iter()
        .find_map(|(_, stmt)| match stmt {
            nova_hir::hir::Stmt::Expr { expr, .. } => Some(*expr),
            _ => None,
        })
        .expect("expected expression statement with new expression");

    assert!(
        matches!(&body.exprs[new_expr], nova_hir::hir::Expr::New { .. }),
        "expected expression statement to be a New expr, got {:?}",
        body.exprs[new_expr]
    );

    let call_site = FileExprId {
        owner: DefWithBodyId::Method(m_id),
        expr: new_expr,
    };

    db.clear_query_stats();
    let resolved = db
        .resolve_method_call_demand(file, call_site)
        .expect("expected constructor call resolution");

    assert_eq!(resolved.name, "<init>");
    assert_eq!(resolved.params, vec![Type::Primitive(PrimitiveType::Int)]);

    let stats = db.query_stats();
    let typeck_body_activity = stats
        .by_query
        .get("typeck_body")
        .map(|s| (s.executions, s.validated_memoized))
        .unwrap_or((0, 0));
    assert_eq!(
        typeck_body_activity,
        (0, 0),
        "resolve_method_call_demand should not invoke full-body type checking"
    );
}

#[test]
fn resolve_method_call_demand_resolves_record_canonical_constructor_call() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();
    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let file_record = FileId::from_raw(1);
    let file_use = FileId::from_raw(2);

    set_file(
        &mut db,
        project,
        file_record,
        "src/R.java",
        "record R(int x) {}",
    );
    set_file(
        &mut db,
        project,
        file_use,
        "src/Use.java",
        r#"
class Use {
    void m(){ new R(1); }
}
"#,
    );
    db.set_project_files(project, Arc::new(vec![file_record, file_use]));

    // Find the `new R(1)` expression inside `Use.m`.
    let tree = db.hir_item_tree(file_use);
    let m_id = find_method_named(&tree, "m");
    let body = db.hir_body(m_id);
    let new_expr = body
        .stmts
        .iter()
        .find_map(|(_, stmt)| match stmt {
            nova_hir::hir::Stmt::Expr { expr, .. } => Some(*expr),
            _ => None,
        })
        .expect("expected expression statement with new expression");

    assert!(
        matches!(&body.exprs[new_expr], nova_hir::hir::Expr::New { .. }),
        "expected expression statement to be a New expr, got {:?}",
        body.exprs[new_expr]
    );

    let call_site = FileExprId {
        owner: DefWithBodyId::Method(m_id),
        expr: new_expr,
    };

    db.clear_query_stats();
    let resolved = db
        .resolve_method_call_demand(file_use, call_site)
        .expect("expected record constructor call resolution");

    assert_eq!(resolved.name, "<init>");
    assert_eq!(resolved.params, vec![Type::Primitive(PrimitiveType::Int)]);

    let stats = db.query_stats();
    let typeck_body_activity = stats
        .by_query
        .get("typeck_body")
        .map(|s| (s.executions, s.validated_memoized))
        .unwrap_or((0, 0));
    assert_eq!(
        typeck_body_activity,
        (0, 0),
        "resolve_method_call_demand should not invoke full-body type checking"
    );
}

#[test]
fn resolve_method_call_demand_resolves_record_varargs_canonical_constructor_call() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();
    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let file_record = FileId::from_raw(1);
    let file_use = FileId::from_raw(2);

    set_file(
        &mut db,
        project,
        file_record,
        "src/R.java",
        "record R(int... xs) {}",
    );
    set_file(
        &mut db,
        project,
        file_use,
        "src/Use.java",
        r#"
class Use {
    void m(){ new R(1, 2, 3); }
}
"#,
    );
    db.set_project_files(project, Arc::new(vec![file_record, file_use]));

    // Find the `new R(1, 2, 3)` expression inside `Use.m`.
    let tree = db.hir_item_tree(file_use);
    let m_id = find_method_named(&tree, "m");
    let body = db.hir_body(m_id);
    let new_expr = body
        .stmts
        .iter()
        .find_map(|(_, stmt)| match stmt {
            nova_hir::hir::Stmt::Expr { expr, .. } => Some(*expr),
            _ => None,
        })
        .expect("expected expression statement with new expression");

    assert!(
        matches!(&body.exprs[new_expr], nova_hir::hir::Expr::New { .. }),
        "expected expression statement to be a New expr, got {:?}",
        body.exprs[new_expr]
    );

    let call_site = FileExprId {
        owner: DefWithBodyId::Method(m_id),
        expr: new_expr,
    };

    db.clear_query_stats();
    let resolved = db
        .resolve_method_call_demand(file_use, call_site)
        .expect("expected record varargs constructor call resolution");

    assert_eq!(resolved.name, "<init>");
    assert!(resolved.is_varargs);
    assert!(resolved.used_varargs);
    assert_eq!(
        resolved.params,
        vec![
            Type::Primitive(PrimitiveType::Int),
            Type::Primitive(PrimitiveType::Int),
            Type::Primitive(PrimitiveType::Int)
        ]
    );
    assert_eq!(
        resolved.signature_params,
        Some(vec![Type::Array(Box::new(Type::Primitive(
            PrimitiveType::Int
        )))])
    );

    let stats = db.query_stats();
    let typeck_body_activity = stats
        .by_query
        .get("typeck_body")
        .map(|s| (s.executions, s.validated_memoized))
        .unwrap_or((0, 0));
    assert_eq!(
        typeck_body_activity,
        (0, 0),
        "resolve_method_call_demand should not invoke full-body type checking"
    );
}

#[test]
fn ambiguous_constructor_call_reports_diag() {
    let src = r#"
class C {
    C(java.lang.Integer x) {}
    C(java.lang.Long x) {}
    void m(){ new C(null); }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "ambiguous-constructor"),
        "expected ambiguous-constructor diagnostic; got {diags:?}"
    );
}

#[test]
fn class_ids_are_stable_across_files_for_jdk_nested_types_referenced_in_expr_position() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();
    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let file_a = FileId::from_raw(1);
    let file_b = FileId::from_raw(2);

    // `Map$Entry` is not part of `TypeStore::with_minimal_jdk()`. This test ensures that
    // `project_base_type_store` still pre-interns nested types that are referenced only in
    // *expression* position (here: as the target of a class literal).
    set_file(
        &mut db,
        project,
        file_a,
        "src/A.java",
        r#"
import java.util.Map;
class A {
    void m() {
        Object x = Map.Entry.class;
    }
}
"#,
    );
    set_file(
        &mut db,
        project,
        file_b,
        "src/B.java",
        r#"
import java.util.Map;
class B {
    void m() {
        Object x = Map.Entry.class;
    }
}
"#,
    );
    db.set_project_files(project, Arc::new(vec![file_a, file_b]));

    let body_a = db.typeck_body(first_method_with_body(&db, file_a));
    let body_b = db.typeck_body(first_method_with_body(&db, file_b));

    let entry_a = body_a
        .env
        .lookup_class("java.util.Map$Entry")
        .expect("expected java.util.Map$Entry to be interned in env");
    let entry_b = body_b
        .env
        .lookup_class("java.util.Map$Entry")
        .expect("expected java.util.Map$Entry to be interned in env");
    assert_eq!(
        entry_a, entry_b,
        "expected stable ClassId for nested type java.util.Map$Entry"
    );
}

#[test]
fn class_ids_are_stable_across_files_for_classpath_types() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();
    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));

    let classdir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../nova-classpath/testdata/classdir");
    let classpath = ClasspathIndex::build(&[ClasspathEntry::ClassDir(classdir)], None).unwrap();
    db.set_classpath_index(project, Some(ArcEq::new(Arc::new(classpath))));

    let file_a = FileId::from_raw(1);
    let file_b = FileId::from_raw(2);

    set_file(
        &mut db,
        project,
        file_a,
        "src/C1.java",
        r#"
import com.example.dep.Bar;
import com.example.dep.Foo;

class C1 {
    void a(Bar b) {}
    void b(Foo f) {}
}
"#,
    );
    set_file(
        &mut db,
        project,
        file_b,
        "src/C2.java",
        r#"
import com.example.dep.Foo;

class C2 {
    void m(Foo f) {}
}
"#,
    );
    db.set_project_files(project, Arc::new(vec![file_a, file_b]));

    let body_a = db.typeck_body(first_method_with_body(&db, file_a));
    let body_b = db.typeck_body(first_method_with_body(&db, file_b));

    let foo_a = body_a
        .env
        .lookup_class("com.example.dep.Foo")
        .expect("expected com.example.dep.Foo to be interned in env");
    let foo_b = body_b
        .env
        .lookup_class("com.example.dep.Foo")
        .expect("expected com.example.dep.Foo to be interned in env");
    assert_eq!(
        foo_a, foo_b,
        "expected stable ClassId for classpath type com.example.dep.Foo"
    );
}

#[test]
fn var_without_initializer_is_error() {
    let src = r#"
class C {
    void m() {
        var x;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "invalid-var"),
        "expected invalid-var diagnostic; got {diags:?}"
    );
}

#[test]
fn var_initialized_with_null_is_error() {
    let src = r#"
class C {
    void m() {
        var x = null;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "invalid-var"),
        "expected invalid-var diagnostic; got {diags:?}"
    );
}

#[test]
fn var_initialized_with_lambda_is_error() {
    let src = r#"
class C {
    void m() {
        var f = (s) -> s;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "var-poly-expression"),
        "expected var-poly-expression diagnostic; got {diags:?}"
    );
}

#[test]
fn void_local_variable_type_is_error() {
    let src = r#"
class C {
    void m() {
        void x = 1;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "void-variable-type"),
        "expected void-variable-type diagnostic; got {diags:?}"
    );
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "invalid-void-type"),
        "expected invalid-void-type diagnostic; got {diags:?}"
    );
}

#[test]
fn void_parameter_type_is_error() {
    let src = r#"
class C {
    void m(void x) {}
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "void-parameter-type"),
        "expected void-parameter-type diagnostic; got {diags:?}"
    );
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "invalid-void-type"),
        "expected invalid-void-type diagnostic; got {diags:?}"
    );
}

#[test]
fn void_parameter_type_is_error_in_abstract_method_signature() {
    let src = r#"
interface I {
    void m(void x);
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "void-parameter-type"),
        "expected void-parameter-type diagnostic; got {diags:?}"
    );
}

#[test]
fn void_catch_parameter_type_is_error() {
    let src = r#"
class C {
    void m() {
        try { } catch (void e) { }
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "void-catch-parameter-type"),
        "expected void-catch-parameter-type diagnostic; got {diags:?}"
    );
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "invalid-void-type"),
        "expected invalid-void-type diagnostic; got {diags:?}"
    );
}

#[test]
fn catch_parameter_var_is_error() {
    let src = r#"
class C {
    void m() {
        try { } catch (var e) { }
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "unresolved-type" && d.message.contains("var")),
        "expected unresolved-type diagnostic for `var` catch parameter; got {diags:?}"
    );
}

#[test]
fn catch_allows_classpath_throwable_subclass() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();
    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));

    // `com.example.MyException extends java.lang.Throwable` supplied via classpath stubs.
    let exc_stub = nova_classpath::ClasspathClassStub {
        binary_name: "com.example.MyException".to_string(),
        internal_name: "com/example/MyException".to_string(),
        access_flags: 0,
        super_binary_name: Some("java.lang.Throwable".to_string()),
        interfaces: Vec::new(),
        signature: None,
        annotations: Vec::new(),
        fields: Vec::new(),
        methods: Vec::new(),
    };
    let module_aware =
        nova_classpath::ModuleAwareClasspathIndex::from_stubs(vec![(exc_stub, None)]);
    db.set_classpath_index(
        project,
        Some(ArcEq::new(Arc::new(module_aware.types.clone()))),
    );

    let src = r#"
class C {
    void m() {
        try { } catch (com.example.MyException e) { }
    }
}
"#;

    let file = FileId::from_raw(1);
    set_file(&mut db, project, file, "src/Test.java", src);
    db.set_project_files(project, Arc::new(vec![file]));

    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.code.as_ref() != "invalid-catch-type"),
        "expected no invalid-catch-type diagnostic for classpath Throwable subclass; got {diags:?}"
    );
    assert!(
        diags.iter().all(|d| !(d.code.as_ref() == "unresolved-type"
            && d.message.contains("com.example.MyException"))),
        "expected com.example.MyException to resolve from classpath; got {diags:?}"
    );
}

#[test]
fn throw_allows_classpath_throwable_subclass() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();
    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));

    // `com.example.MyException extends java.lang.Throwable` supplied via classpath stubs.
    let exc_stub = nova_classpath::ClasspathClassStub {
        binary_name: "com.example.MyException".to_string(),
        internal_name: "com/example/MyException".to_string(),
        access_flags: 0,
        super_binary_name: Some("java.lang.Throwable".to_string()),
        interfaces: Vec::new(),
        signature: None,
        annotations: Vec::new(),
        fields: Vec::new(),
        methods: Vec::new(),
    };
    let module_aware =
        nova_classpath::ModuleAwareClasspathIndex::from_stubs(vec![(exc_stub, None)]);
    db.set_classpath_index(
        project,
        Some(ArcEq::new(Arc::new(module_aware.types.clone()))),
    );

    let src = r#"
class C {
    void m() {
        throw new com.example.MyException();
    }
}
"#;

    let file = FileId::from_raw(1);
    set_file(&mut db, project, file, "src/Test.java", src);
    db.set_project_files(project, Arc::new(vec![file]));

    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "invalid-throw"),
        "expected no invalid-throw diagnostic for classpath Throwable subclass; got {diags:?}"
    );
}

#[test]
fn diamond_inference_for_new() {
    let src = r#"
import java.util.*;
class C { void m(){ List<String> xs = new ArrayList<>(); } }
"#;

    let (db, file) = setup_db(src);
    let offset = src
        .find("new ArrayList")
        .expect("snippet should contain new ArrayList")
        + "new ".len()
        + "Array".len();
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "ArrayList<String>");
}

#[test]
fn diamond_inference_uses_target_type_from_assignment() {
    let src = r#"
import java.util.*;
class C {
    void m() {
        List<String> xs;
        xs = new ArrayList<>();
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "type-mismatch"),
        "expected no type-mismatch diagnostics; got {diags:?}"
    );

    let offset = src
        .find("new ArrayList")
        .expect("snippet should contain new ArrayList")
        + "new ".len()
        + "Array".len();
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "ArrayList<String>");
}
