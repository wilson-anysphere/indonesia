use nova_refactor::extract_method::{ExtractMethod, InsertionStrategy, Visibility};
use nova_refactor::{apply_workspace_edit, FileId, WorkspaceEdit};
use nova_test_utils::extract_range;
use std::collections::BTreeMap;

fn apply_single_file(file: &str, source: &str, edit: &WorkspaceEdit) -> String {
    let mut files = BTreeMap::new();
    let file_id = FileId::new(file.to_string());
    files.insert(file_id.clone(), source.to_string());
    let out = apply_workspace_edit(&files, edit).expect("apply workspace edit");
    out.get(&file_id).cloned().expect("file must exist")
}

fn assert_no_overlaps(edit: &WorkspaceEdit) {
    let mut normalized = edit.clone();
    normalized
        .normalize()
        .expect("edits should normalize without overlaps");
}

fn assert_crlf_only(text: &str) {
    let bytes = text.as_bytes();
    for (idx, b) in bytes.iter().enumerate() {
        if *b == b'\n' {
            assert!(
                idx > 0 && bytes[idx - 1] == b'\r',
                "found bare LF at byte offset {idx}"
            );
        }
        if *b == b'\r' {
            assert!(
                idx + 1 < bytes.len() && bytes[idx + 1] == b'\n',
                "found bare CR at byte offset {idx}"
            );
        }
    }
}

#[test]
fn extract_method_with_parameters() {
    let fixture = r#"
class C {
    void m(int a) {
        int b = 1;
        /*start*/System.out.println(a + b);/*end*/
        System.out.println("done");
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "extracted".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let edit = refactoring.apply(&source).expect("apply should succeed");
    assert_no_overlaps(&edit);
    let actual = apply_single_file("Main.java", &source, &edit);

    let expected = r#"
class C {
    void m(int a) {
        int b = 1;
        extracted(a, b);
        System.out.println("done");
    }

    private void extracted(int a, int b) {
        System.out.println(a + b);
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_switch_expression_arm_reads_become_parameters() {
    // Regression test: locals referenced only inside switch expression arms must still be
    // discovered by flow-based parameter analysis.
    let fixture = r#"
class C {
    void m(int x) {
        /*start*/System.out.println(switch (0) { case 0 -> x + 1; default -> x + 2; });/*end*/
        System.out.println("done");
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "extracted".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let edit = refactoring.apply(&source).expect("apply should succeed");
    assert_no_overlaps(&edit);
    let actual = apply_single_file("Main.java", &source, &edit);

    let expected = r#"
class C {
    void m(int x) {
        extracted(x);
        System.out.println("done");
    }

    private void extracted(int x) {
        System.out.println(switch (0) { case 0 -> x + 1; default -> x + 2; });
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_from_interface_default_method() {
    let fixture = r#"
interface I {
    default void m(int a) {
        int b = 1;
        /*start*/System.out.println(a + b);/*end*/
        System.out.println("done");
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "extracted".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::EndOfClass,
    };

    let edit = refactoring.apply(&source).expect("apply should succeed");
    assert_no_overlaps(&edit);
    let actual = apply_single_file("Main.java", &source, &edit);

    let expected = r#"
interface I {
    default void m(int a) {
        int b = 1;
        extracted(a, b);
        System.out.println("done");
    }

    private void extracted(int a, int b) {
        System.out.println(a + b);
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_from_enum_instance_method() {
    let fixture = r#"
enum E {
    A;

    void m(int a) {
        int b = 1;
        /*start*/System.out.println(a + b);/*end*/
        System.out.println("done");
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "extracted".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::EndOfClass,
    };

    let edit = refactoring.apply(&source).expect("apply should succeed");
    assert_no_overlaps(&edit);
    let actual = apply_single_file("Main.java", &source, &edit);

    let expected = r#"
enum E {
    A;

    void m(int a) {
        int b = 1;
        extracted(a, b);
        System.out.println("done");
    }

    private void extracted(int a, int b) {
        System.out.println(a + b);
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_from_record_method() {
    let fixture = r#"
record R(int x) {
    void m(int a) {
        int b = 1;
        /*start*/System.out.println(a + b);/*end*/
        System.out.println("done");
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "extracted".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::EndOfClass,
    };

    let edit = refactoring.apply(&source).expect("apply should succeed");
    assert_no_overlaps(&edit);
    let actual = apply_single_file("Main.java", &source, &edit);

    let expected = r#"
record R(int x) {
    void m(int a) {
        int b = 1;
        extracted(a, b);
        System.out.println("done");
    }

    private void extracted(int a, int b) {
        System.out.println(a + b);
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_preserves_static_context() {
    let fixture = r#"
class C {
    static void m(int a) {
        int b = 1;
        /*start*/System.out.println(a + b);/*end*/
        System.out.println("done");
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "extracted".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let edit = refactoring.apply(&source).expect("apply should succeed");
    assert_no_overlaps(&edit);
    let actual = apply_single_file("Main.java", &source, &edit);

    let expected = r#"
class C {
    static void m(int a) {
        int b = 1;
        extracted(a, b);
        System.out.println("done");
    }

    private static void extracted(int a, int b) {
        System.out.println(a + b);
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_copies_enclosing_type_parameters() {
    let fixture = r#"
class C {
    <T> void m(T t) {
        /*start*/System.out.println(t);/*end*/
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "extracted".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let edit = refactoring.apply(&source).expect("apply should succeed");
    assert_no_overlaps(&edit);
    let actual = apply_single_file("Main.java", &source, &edit);

    let expected = r#"
class C {
    <T> void m(T t) {
        extracted(t);
    }

    private <T> void extracted(T t) {
        System.out.println(t);
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_returning_generic_type_variable() {
    let fixture = r#"
class C {
    <T> T m(T t) {
        T r = null;
        /*start*/r = t;/*end*/
        return r;
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "compute".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let edit = refactoring.apply(&source).expect("apply should succeed");
    assert_no_overlaps(&edit);
    let actual = apply_single_file("Main.java", &source, &edit);

    let expected = r#"
class C {
    <T> T m(T t) {
        T r = null;
        r = compute(r, t);
        return r;
    }

    private <T> T compute(T r, T t) {
        r = t;
        return r;
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_returning_value() {
    let fixture = r#"
class C {
    int m(int a) {
        int b = 1;
        int r = 0;
        /*start*/r = a + b;/*end*/
        return r;
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "compute".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let edit = refactoring.apply(&source).expect("apply should succeed");
    assert_no_overlaps(&edit);
    let actual = apply_single_file("Main.java", &source, &edit);

    let expected = r#"
class C {
    int m(int a) {
        int b = 1;
        int r = 0;
        r = compute(r, a, b);
        return r;
    }

    private int compute(int r, int a, int b) {
        r = a + b;
        return r;
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_return_candidate_parameter_is_threaded_through() {
    let fixture = r#"
class C {
    void m(int x, boolean cond) {
        /*start*/if (cond) x = 1;/*end*/
        System.out.println(x);
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "extracted".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let edit = refactoring.apply(&source).expect("apply should succeed");
    assert_no_overlaps(&edit);
    let actual = apply_single_file("Main.java", &source, &edit);

    let expected = r#"
class C {
    void m(int x, boolean cond) {
        x = extracted(x, cond);
        System.out.println(x);
    }

    private int extracted(int x, boolean cond) {
        if (cond) x = 1;
        return x;
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_return_candidate_local_initialized_before_selection() {
    let fixture = r#"
class C {
    void m(boolean cond) {
        int r = 0;
        /*start*/if (cond) r = 1;/*end*/
        System.out.println(r);
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "extracted".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let edit = refactoring.apply(&source).expect("apply should succeed");
    assert_no_overlaps(&edit);
    let actual = apply_single_file("Main.java", &source, &edit);

    let expected = r#"
class C {
    void m(boolean cond) {
        int r = 0;
        r = extracted(r, cond);
        System.out.println(r);
    }

    private int extracted(int r, boolean cond) {
        if (cond) r = 1;
        return r;
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_multiple_statements_with_parameters() {
    let fixture = r#"
class C {
    void m(int a, int b) {
        int x = 1;
        /*start*/System.out.println(b);
        System.out.println(a + x);/*end*/
        System.out.println("done");
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "extracted".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let edit = refactoring.apply(&source).expect("apply should succeed");
    assert_no_overlaps(&edit);
    let actual = apply_single_file("Main.java", &source, &edit);

    let expected = r#"
class C {
    void m(int a, int b) {
        int x = 1;
        extracted(b, a, x);
        System.out.println("done");
    }

    private void extracted(int b, int a, int x) {
        System.out.println(b);
        System.out.println(a + x);
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_inside_constructor() {
    let fixture = r#"
class C {
    C(int a) {
        int b = 1;
        /*start*/System.out.println(a + b);/*end*/
    }

    void m() {
        System.out.println("other");
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "extracted".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let edit = refactoring.apply(&source).expect("apply should succeed");
    assert_no_overlaps(&edit);
    let actual = apply_single_file("Main.java", &source, &edit);

    let expected = r#"
class C {
    C(int a) {
        int b = 1;
        extracted(a, b);
    }

    private void extracted(int a, int b) {
        System.out.println(a + b);
    }

    void m() {
        System.out.println("other");
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_inside_instance_initializer_block() {
    let fixture = r#"
class C {
    {
        /*start*/System.out.println(1);/*end*/
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "extracted".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let edit = refactoring.apply(&source).expect("apply should succeed");
    assert_no_overlaps(&edit);
    let actual = apply_single_file("Main.java", &source, &edit);

    let expected = r#"
class C {
    {
        extracted();
    }

    private void extracted() {
        System.out.println(1);
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_inside_static_initializer_block() {
    let fixture = r#"
class C {
    static {
        int b = 1;
        /*start*/System.out.println(b);/*end*/
    }

    void m() {
        System.out.println("other");
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "extracted".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let edit = refactoring.apply(&source).expect("apply should succeed");
    assert_no_overlaps(&edit);
    let actual = apply_single_file("Main.java", &source, &edit);

    let expected = r#"
class C {
    static {
        int b = 1;
        extracted(b);
    }

    private static void extracted(int b) {
        System.out.println(b);
    }

    void m() {
        System.out.println("other");
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_rejects_illegal_control_flow() {
    let fixture = r#"
class C {
    int m() {
        /*start*/return 1;/*end*/
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "bad".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let err = refactoring
        .apply(&source)
        .expect_err("should reject selection");
    assert!(
        err.contains("IllegalControlFlow"),
        "unexpected error: {err}"
    );
}

#[test]
fn extract_method_rejects_reference_to_local_type_declared_in_enclosing_body() {
    let fixture = r#"
class C {
    void m() {
        class Local {}
        /*start*/Local x = new Local();/*end*/
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "extracted".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let err = refactoring
        .apply(&source)
        .expect_err("should reject selection");
    assert!(
        err.contains("ReferencesLocalType"),
        "unexpected error: {err}"
    );
    assert!(err.contains("Local"), "unexpected error: {err}");
}

#[test]
fn extract_method_rejects_reference_to_local_enum_declared_in_enclosing_body() {
    let fixture = r#"
class C {
    void m() {
        enum Local { A }
        /*start*/Local x = Local.A;/*end*/
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "extracted".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let err = refactoring
        .apply(&source)
        .expect_err("should reject selection");
    assert!(
        err.contains("ReferencesLocalType"),
        "unexpected error: {err}"
    );
    assert!(err.contains("Local"), "unexpected error: {err}");
}

#[test]
fn extract_method_rejects_reference_to_local_record_declared_in_enclosing_body() {
    let fixture = r#"
class C {
    void m() {
        record Local(int x) {}
        /*start*/Local y = new Local(1);/*end*/
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "extracted".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let err = refactoring
        .apply(&source)
        .expect_err("should reject selection");
    assert!(
        err.contains("ReferencesLocalType"),
        "unexpected error: {err}"
    );
    assert!(err.contains("Local"), "unexpected error: {err}");
}

#[test]
fn extract_method_rejects_void_return_statement() {
    let fixture = r#"
class C {
    void m() {
        /*start*/return;/*end*/
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "bad".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let err = refactoring
        .apply(&source)
        .expect_err("should reject selection");
    assert!(
        err.contains("IllegalControlFlow"),
        "unexpected error: {err}"
    );
    assert!(err.contains("Return"), "unexpected error: {err}");
}

#[test]
fn extract_method_rejects_nested_return() {
    let fixture = r#"
class C {
    int m(int a) {
        /*start*/if (a > 0) {
            return 1;
        }/*end*/
        return 0;
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "bad".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let err = refactoring
        .apply(&source)
        .expect_err("should reject selection");
    assert!(
        err.contains("IllegalControlFlow"),
        "unexpected error: {err}"
    );
}

#[test]
fn extract_method_rejects_selection_inside_lambda_body() {
    let fixture = r#"
class C {
    void m() {
        Runnable r = () -> {
            /*start*/System.out.println(1);/*end*/
        };
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "extracted".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let err = refactoring
        .apply(&source)
        .expect_err("should reject selection inside lambda body");
    assert!(err.contains("InvalidSelection"));
}

#[test]
fn extract_method_allows_lambda_with_return() {
    let fixture = r#"
class C {
    void m() {
        /*start*/Runnable r = () -> { return; };/*end*/
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "extracted".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let edit = refactoring.apply(&source).expect("apply should succeed");
    assert_no_overlaps(&edit);
    let actual = apply_single_file("Main.java", &source, &edit);

    let expected = r#"
class C {
    void m() {
        extracted();
    }

    private void extracted() {
        Runnable r = () -> { return; };
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_rejects_yield_statement() {
    let fixture = r#"
class C {
  int m(int x) {
    return switch (x) {
      case 0 -> {
        /*start*/yield 1;/*end*/
      }
      default -> 2;
    };
  }
}
"#;

    let (source, selection) = extract_range(fixture);
    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "bad".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let err = refactoring
        .apply(&source)
        .expect_err("should reject yield selection");
    assert!(err.contains("IllegalControlFlow"));
    assert!(err.contains("Yield"));
}

#[test]
fn edits_are_non_overlapping() {
    // Construct a selection whose replacement and insertion could overlap if offsets
    // were computed incorrectly.
    let fixture = r#"
class C {
    void m(int a) {
        /*start*/System.out.println(a);/*end*/
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "log".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let edit = refactoring.apply(&source).expect("apply should succeed");
    assert_no_overlaps(&edit);
    let _ = apply_single_file("Main.java", &source, &edit);
}

#[test]
fn extract_method_uses_try_with_resources_variable_as_parameter() {
    let fixture = r#"
class C {
    void m() {
        try (java.io.ByteArrayInputStream in = new java.io.ByteArrayInputStream(new byte[0])) {
            /*start*/System.out.println(in);/*end*/
        }
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "extracted".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let edit = refactoring.apply(&source).expect("apply should succeed");
    assert_no_overlaps(&edit);
    let actual = apply_single_file("Main.java", &source, &edit);

    let expected = r#"
class C {
    void m() {
        try (java.io.ByteArrayInputStream in = new java.io.ByteArrayInputStream(new byte[0])) {
            extracted(in);
        }
    }

    private void extracted(java.io.ByteArrayInputStream in) {
        System.out.println(in);
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_adds_throws_for_explicit_throw() {
    let fixture = r#"
class C {
    void m() {
        /*start*/throw new RuntimeException();/*end*/
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "boom".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let edit = refactoring.apply(&source).expect("apply should succeed");
    assert_no_overlaps(&edit);
    let actual = apply_single_file("Main.java", &source, &edit);

    let expected = r#"
class C {
    void m() {
        boom();
    }

    private void boom() throws RuntimeException {
        throw new RuntimeException();
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_adds_throws_for_catch_param_throw() {
    let fixture = r#"
class C {
    void m() throws java.io.IOException {
        try {
            throw new java.io.IOException();
        } catch (java.io.IOException e) {
            /*start*/throw e;/*end*/
        }
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "rethrow".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let edit = refactoring.apply(&source).expect("apply should succeed");
    assert_no_overlaps(&edit);
    let actual = apply_single_file("Main.java", &source, &edit);

    let expected = r#"
class C {
    void m() throws java.io.IOException {
        try {
            throw new java.io.IOException();
        } catch (java.io.IOException e) {
            rethrow(e);
        }
    }

    private void rethrow(java.io.IOException e) throws java.io.IOException {
        throw e;
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_adds_throws_for_resource_var_throw() {
    let fixture = r#"
class C {
    void m() throws MyException {
        try (MyException in = new MyException()) {
            /*start*/throw in;/*end*/
        }
    }

    static class MyException extends Exception implements AutoCloseable {
        @Override public void close() {}
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "throwIt".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let edit = refactoring.apply(&source).expect("apply should succeed");
    assert_no_overlaps(&edit);
    let actual = apply_single_file("Main.java", &source, &edit);

    let expected = r#"
class C {
    void m() throws MyException {
        try (MyException in = new MyException()) {
            throwIt(in);
        }
    }

    private void throwIt(MyException in) throws MyException {
        throw in;
    }

    static class MyException extends Exception implements AutoCloseable {
        @Override public void close() {}
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_rejects_keyword_method_name() {
    let fixture = r#"
class C {
    void m(int a) {
        int b = 1;
        /*start*/System.out.println(a + b);/*end*/
        System.out.println("done");
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "class".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let err = refactoring
        .apply(&source)
        .expect_err("should reject invalid method name");
    assert!(err.contains("InvalidMethodName"));
}

#[test]
fn extract_method_rejects_non_identifier_method_name() {
    let fixture = r#"
class C {
    void m(int a) {
        int b = 1;
        /*start*/System.out.println(a + b);/*end*/
        System.out.println("done");
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "1foo".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let err = refactoring
        .apply(&source)
        .expect_err("should reject invalid method name");
    assert!(err.contains("InvalidMethodName"));
}

#[test]
fn extract_method_allows_underscore_in_method_name() {
    let fixture = r#"
class C {
    void m(int a) {
        int b = 1;
        /*start*/System.out.println(a + b);/*end*/
        System.out.println("done");
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "foo_bar".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let edit = refactoring.apply(&source).expect("apply should succeed");
    assert_no_overlaps(&edit);
    let actual = apply_single_file("Main.java", &source, &edit);

    let expected = r#"
class C {
    void m(int a) {
        int b = 1;
        foo_bar(a, b);
        System.out.println("done");
    }

    private void foo_bar(int a, int b) {
        System.out.println(a + b);
    }
}
"#;
    assert_eq!(actual, expected);
}

#[test]
fn extract_method_preserves_crlf_newlines() {
    let fixture_lf = r#"
class C {
    void m(int a) {
        int b = 1;
        /*start*/System.out.println(a + b);/*end*/
        System.out.println("done");
    }

    void n() {
        System.out.println("n");
    }
}
"#;

    let fixture = fixture_lf.replace('\n', "\r\n");
    let (source, selection) = extract_range(&fixture);
    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "extracted".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::EndOfClass,
    };

    let edit = refactoring.apply(&source).expect("apply should succeed");
    assert_no_overlaps(&edit);
    let actual = apply_single_file("Main.java", &source, &edit);

    assert_crlf_only(&actual);

    let expected_lf = r#"
class C {
    void m(int a) {
        int b = 1;
        extracted(a, b);
        System.out.println("done");
    }

    void n() {
        System.out.println("n");
    }

    private void extracted(int a, int b) {
        System.out.println(a + b);
    }
}
"#;
    let expected = expected_lf.replace('\n', "\r\n");
    assert_eq!(actual, expected);
}

#[test]
fn extract_method_multi_statement_with_declared_return_value() {
    let fixture = r#"
class C {
    void m(int a) {
        /*start*/int tmp = a + 1;
        System.out.println(tmp);/*end*/
        System.out.println(tmp + 2);
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "initTmp".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let edit = refactoring.apply(&source).expect("apply should succeed");
    assert_no_overlaps(&edit);
    let actual = apply_single_file("Main.java", &source, &edit);

    let expected = r#"
class C {
    void m(int a) {
        int tmp = initTmp(a);
        System.out.println(tmp + 2);
    }

    private int initTmp(int a) {
        int tmp = a + 1;
        System.out.println(tmp);
        return tmp;
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_preserves_final_on_declared_return_value() {
    let fixture = r#"
class C {
    void m() {
        /*start*/final int x = 1;/*end*/ // comment
        System.out.println(x);
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "initX".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let edit = refactoring.apply(&source).expect("apply should succeed");
    assert_no_overlaps(&edit);
    let actual = apply_single_file("Main.java", &source, &edit);

    let expected = r#"
class C {
    void m() {
        final int x = initX(); // comment
        System.out.println(x);
    }

    private int initX() {
        final int x = 1;
        return x;
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_rejects_break_when_target_is_outside_selection() {
    let fixture = r#"
class C {
    void m() {
        while (true) {
            /*start*/break;/*end*/
        }
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "bad".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let err = refactoring
        .apply(&source)
        .expect_err("should reject selection");
    assert!(
        err.contains("IllegalControlFlow"),
        "unexpected error: {err}"
    );
    assert!(err.contains("Break"), "unexpected error: {err}");
}

#[test]
fn extract_method_liveness_through_loop_condition() {
    let fixture = r#"
class C {
    void m() {
        int i = 0;
        while (i < 3) {
            /*start*/i = i + 1;/*end*/
        }
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "inc".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let edit = refactoring.apply(&source).expect("apply should succeed");
    assert_no_overlaps(&edit);
    let actual = apply_single_file("Main.java", &source, &edit);

    let expected = r#"
class C {
    void m() {
        int i = 0;
        while (i < 3) {
            i = inc(i);
        }
    }

    private int inc(int i) {
        i = i + 1;
        return i;
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_from_expression_in_return() {
    let fixture = r#"
class C {
    int m(int a, int b) {
        return /*start*/a + b/*end*/;
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "sum".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let edit = refactoring.apply(&source).expect("apply should succeed");
    assert_no_overlaps(&edit);
    let actual = apply_single_file("Main.java", &source, &edit);

    let expected = r#"
class C {
    int m(int a, int b) {
        return sum(a, b);
    }

    private int sum(int a, int b) {
        return a + b;
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_rejects_mutating_expression() {
    let fixture = r#"
class C {
    int m(int i) {
        return /*start*/i++/*end*/;
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "bad".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let err = refactoring
        .apply(&source)
        .expect_err("should reject selection");
    assert!(err.contains("InvalidSelection"));
}

#[test]
fn extract_method_in_catch_block_uses_catch_param_type() {
    let fixture = r#"
class C {
    void m() {
        try {
            System.out.println("ok");
        } catch (RuntimeException e) {
            /*start*/System.out.println(e.getMessage());/*end*/
        }
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "handle".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let edit = refactoring.apply(&source).expect("apply should succeed");
    assert_no_overlaps(&edit);
    let actual = apply_single_file("Main.java", &source, &edit);

    let expected = r#"
class C {
    void m() {
        try {
            System.out.println("ok");
        } catch (RuntimeException e) {
            handle(e);
        }
    }

    private void handle(RuntimeException e) {
        System.out.println(e.getMessage());
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_infers_var_parameter_type() {
    let fixture = r#"
class C {
    void m() {
        var x = 1;
        /*start*/System.out.println(x);/*end*/
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "extracted".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let edit = refactoring.apply(&source).expect("apply should succeed");
    assert_no_overlaps(&edit);
    let actual = apply_single_file("Main.java", &source, &edit);

    let expected = r#"
class C {
    void m() {
        var x = 1;
        extracted(x);
    }

    private void extracted(int x) {
        System.out.println(x);
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_infers_var_return_type() {
    let fixture = r#"
class C {
    void m() {
        /*start*/var x = 1;/*end*/
        System.out.println(x);
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "extracted".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let edit = refactoring.apply(&source).expect("apply should succeed");
    assert_no_overlaps(&edit);
    let actual = apply_single_file("Main.java", &source, &edit);

    let expected = r#"
class C {
    void m() {
        int x = extracted();
        System.out.println(x);
    }

    private int extracted() {
        var x = 1;
        return x;
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_rejects_protected_visibility_in_interface() {
    let fixture = r#"
interface I {
    default void m(int a) {
        int b = 1;
        /*start*/System.out.println(a + b);/*end*/
        System.out.println("done");
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "extracted".to_string(),
        visibility: Visibility::Protected,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let err = refactoring
        .apply(&source)
        .expect_err("should reject protected visibility in interface context");
    assert!(
        err.contains("InvalidVisibilityForInterface"),
        "unexpected error: {err}"
    );
}

#[test]
fn extract_method_inside_interface_with_public_visibility_emits_default() {
    let fixture = r#"
interface I {
    default void m(int a) {
        int b = 1;
        /*start*/System.out.println(a + b);/*end*/
        System.out.println("done");
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "extracted".to_string(),
        visibility: Visibility::Public,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let edit = refactoring.apply(&source).expect("apply should succeed");
    assert_no_overlaps(&edit);
    let actual = apply_single_file("Main.java", &source, &edit);

    let expected = r#"
interface I {
    default void m(int a) {
        int b = 1;
        extracted(a, b);
        System.out.println("done");
    }

    public default void extracted(int a, int b) {
        System.out.println(a + b);
    }
}
"#;

    assert_eq!(actual, expected);
}
