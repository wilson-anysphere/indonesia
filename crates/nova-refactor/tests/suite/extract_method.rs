use nova_refactor::extract_method::{
    ExtractMethod, ExtractMethodIssue, InsertionStrategy, Visibility,
};
use nova_refactor::{apply_workspace_edit, FileId, TextRange, WorkspaceEdit};
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
fn extract_method_analyze_rejects_selection_start_past_eof_without_panicking() {
    let source = "class C { void m() { int x = 1; } }\n";
    let selection = TextRange::new(source.len() + 5, source.len());

    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "extracted".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let analysis = refactoring
        .analyze(source)
        .expect("analyze should not error");
    assert!(
        analysis
            .issues
            .contains(&ExtractMethodIssue::InvalidSelection),
        "expected InvalidSelection issue; got {:?}",
        analysis.issues
    );
}

#[test]
fn extract_method_analyze_rejects_selection_end_past_eof_without_panicking() {
    let source = "class C { void m() { int x = 1; } }\n";
    let selection = TextRange::new(0, source.len() + 5);

    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "extracted".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let analysis = refactoring
        .analyze(source)
        .expect("analyze should not error");
    assert!(
        analysis
            .issues
            .contains(&ExtractMethodIssue::InvalidSelection),
        "expected InvalidSelection issue; got {:?}",
        analysis.issues
    );
}

#[test]
fn extract_method_allows_overloading() {
    let fixture = r#"
class C {
    void m(int a) {
        /*start*/System.out.println(a);/*end*/
    }

    private void extracted(String s) {
        System.out.println(s);
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
        extracted(a);
    }

    private void extracted(int a) {
        System.out.println(a);
    }

    private void extracted(String s) {
        System.out.println(s);
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_rejects_true_signature_collision() {
    let fixture = r#"
class C {
    void m(int a) {
        /*start*/System.out.println(a);/*end*/
    }

    private void extracted(int a) {
        System.out.println(a);
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

    let analysis = refactoring
        .analyze(&source)
        .expect("analysis should succeed");
    assert!(
        analysis.issues.iter().any(|issue| matches!(
            issue,
            ExtractMethodIssue::NameCollision { name } if name == "extracted"
        )),
        "expected NameCollision issue, got: {:?}",
        analysis.issues
    );
}

#[test]
fn extract_method_if_without_braces() {
    let fixture = r#"
class C {
    void m(boolean cond, int a) {
        if (cond)
            /*start*/System.out.println(a);/*end*/
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
    void m(boolean cond, int a) {
        if (cond)
            extracted(a);
        System.out.println("done");
    }

    private void extracted(int a) {
        System.out.println(a);
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_while_without_braces() {
    let fixture = r#"
class C {
    void m(boolean cond) {
        int x = 0;
        while (cond)
            /*start*/x = x + 1;/*end*/
        System.out.println(x);
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
    void m(boolean cond) {
        int x = 0;
        while (cond)
            x = inc(x);
        System.out.println(x);
    }

    private int inc(int x) {
        x = x + 1;
        return x;
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_if_else_without_braces() {
    let fixture = r#"
class C {
    void m(boolean cond, int a) {
        if (cond)
            System.out.println("then");
        else
            /*start*/System.out.println(a);/*end*/
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
    void m(boolean cond, int a) {
        if (cond)
            System.out.println("then");
        else
            extracted(a);
        System.out.println("done");
    }

    private void extracted(int a) {
        System.out.println(a);
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_for_without_braces() {
    let fixture = r#"
class C {
    void m(int a) {
        for (int i = 0; i < 1; i++)
            /*start*/System.out.println(a);/*end*/
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
        for (int i = 0; i < 1; i++)
            extracted(a);
        System.out.println("done");
    }

    private void extracted(int a) {
        System.out.println(a);
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_do_while_without_braces() {
    let fixture = r#"
class C {
    void m(boolean cond, int a) {
        do
            /*start*/System.out.println(a);/*end*/
        while (cond);
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
    void m(boolean cond, int a) {
        do
            extracted(a);
        while (cond);
        System.out.println("done");
    }

    private void extracted(int a) {
        System.out.println(a);
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_enhanced_for_without_braces() {
    let fixture = r#"
class C {
    void m(int[] xs) {
        for (int x : xs)
            /*start*/System.out.println(x);/*end*/
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
    void m(int[] xs) {
        for (int x : xs)
            extracted(x);
        System.out.println("done");
    }

    private void extracted(int x) {
        System.out.println(x);
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_switch_case_without_braces() {
    let fixture = r#"
class C {
    void m(int a) {
        switch (0) {
            case 0:
                /*start*/System.out.println(a);/*end*/
                break;
            default:
                System.out.println("other");
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
    void m(int a) {
        switch (0) {
            case 0:
                extracted(a);
                break;
            default:
                System.out.println("other");
        }
    }

    private void extracted(int a) {
        System.out.println(a);
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
fn extract_method_switch_expression_in_local_initializer_discovers_arm_locals() {
    // Similar to the common `int y = switch (...) { ... };` pattern, but the variable `x` is only
    // referenced inside the switch arms (not in the selector). Without proper SwitchExpression
    // lowering in flow IR, Extract Method would miss `x` and generate an invalid extracted method.
    let fixture = r#"
class C {
    int m(int x, int sel) {
        /*start*/int y = switch (sel) { case 0 -> x + 1; default -> x + 2; };/*end*/
        return y;
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
    int m(int x, int sel) {
        int y = extracted(sel, x);
        return y;
    }

    private int extracted(int sel, int x) {
        int y = switch (sel) { case 0 -> x + 1; default -> x + 2; };
        return y;
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
fn extract_method_copies_enclosing_type_parameters_from_generic_constructor() {
    let fixture = r#"
class C {
    <T> C(T t) {
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
    <T> C(T t) {
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
fn extract_method_copies_multiple_enclosing_type_parameters_with_bounds() {
    let fixture = r#"
class C {
    <T, U extends Comparable<U>> void m(T t, U u) {
        /*start*/System.out.println(u);/*end*/
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
    <T, U extends Comparable<U>> void m(T t, U u) {
        extracted(u);
    }

    private <T, U extends Comparable<U>> void extracted(U u) {
        System.out.println(u);
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
fn extract_method_returns_value_when_selection_is_last_in_if_block() {
    let fixture = r#"
class C {
    void m(boolean cond) {
        int x = 0;
        if (cond) {
            /*start*/x = 1;/*end*/
        }
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
    void m(boolean cond) {
        int x = 0;
        if (cond) {
            x = extracted(x);
        }
        System.out.println(x);
    }

    private int extracted(int x) {
        x = 1;
        return x;
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_at_end_of_body_does_not_invent_return_value() {
    let fixture = r#"
class C {
    void m() {
        int x = 0;
        /*start*/x = x + 1;/*end*/
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
        int x = 0;
        extracted(x);
    }

    private void extracted(int x) {
        x = x + 1;
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_return_value_used_via_lambda_capture_after_selection() {
    let fixture = r#"
class C {
    void m() {
        int x = 0;
        /*start*/x = 1;/*end*/
        Runnable r = () -> System.out.println(x);
        r.run();
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
        int x = 0;
        x = extracted(x);
        Runnable r = () -> System.out.println(x);
        r.run();
    }

    private int extracted(int x) {
        x = 1;
        return x;
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_return_value_used_via_anonymous_class_capture_after_selection() {
    let fixture = r#"
class C {
    void m() {
        int x = 0;
        /*start*/x = 1;/*end*/
        Runnable r = new Runnable() {
            public void run() {
                System.out.println(x);
            }
        };
        r.run();
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
        int x = 0;
        x = extracted(x);
        Runnable r = new Runnable() {
            public void run() {
                System.out.println(x);
            }
        };
        r.run();
    }

    private int extracted(int x) {
        x = 1;
        return x;
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
fn extract_method_includes_lambda_captures_as_parameters() {
    let fixture = r#"
class C {
    void m(int x) {
        java.util.List<Integer> xs = java.util.List.of(1);
        /*start*/xs.forEach(i -> System.out.println(x + i));/*end*/
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
        java.util.List<Integer> xs = java.util.List.of(1);
        extracted(xs, x);
    }

    private void extracted(java.util.List<Integer> xs, int x) {
        xs.forEach(i -> System.out.println(x + i));
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_includes_anonymous_class_captures_as_parameters() {
    let fixture = r#"
class C {
    void m(int x) {
        /*start*/new Runnable() { public void run() { System.out.println(x); } }.run();/*end*/
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
    }

    private void extracted(int x) {
        new Runnable() { public void run() { System.out.println(x); } }.run();
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
fn extract_method_inside_record_compact_constructor() {
    let fixture = r#"
record R(int x) {
    R {
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
record R(int x) {
    R {
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
fn extract_method_inside_record_compact_constructor_after_mutation() {
    let fixture = r#"
record R(int x) {
    R {
        x = x + 1;
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
record R(int x) {
    R {
        x = x + 1;
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
fn extract_method_expression_inside_record_compact_constructor() {
    let fixture = r#"
record R(int x) {
    R {
        int y = /*start*/x + 1/*end*/;
        System.out.println(y);
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
record R(int x) {
    R {
        int y = extracted(x);
        System.out.println(y);
    }

    private int extracted(int x) {
        return x + 1;
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
fn extract_method_constructor_rejects_super_invocation_selection() {
    let fixture = r#"
class C {
    C() {
        /*start*/super();/*end*/
        System.out.println("hi");
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
        err.contains("ConstructorInvocation"),
        "unexpected error: {err}"
    );
}

#[test]
fn extract_method_constructor_keeps_super_invocation_first() {
    let fixture = r#"
class C {
    C() {
        super();
        /*start*/System.out.println("hi");/*end*/
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
    C() {
        super();
        extracted();
    }

    private void extracted() {
        System.out.println("hi");
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
fn extract_method_expression_inside_instance_initializer_block() {
    let fixture = r#"
class C {
    {
        int x = /*start*/1 + 2/*end*/;
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
    {
        int x = extracted();
        System.out.println(x);
    }

    private int extracted() {
        return 1 + 2;
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_expression_inside_static_initializer_block() {
    let fixture = r#"
class C {
    static {
        int x = /*start*/1 + 2/*end*/;
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
    static {
        int x = extracted();
        System.out.println(x);
    }

    private static int extracted() {
        return 1 + 2;
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_expression_long_literal_inside_instance_initializer_block() {
    let fixture = r#"
class C {
    {
        long x = /*start*/1L/*end*/;
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
    {
        long x = extracted();
        System.out.println(x);
    }

    private long extracted() {
        return 1L;
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_expression_long_literal_inside_static_initializer_block() {
    let fixture = r#"
class C {
    static {
        long x = /*start*/1L/*end*/;
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
    static {
        long x = extracted();
        System.out.println(x);
    }

    private static long extracted() {
        return 1L;
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_return_value_inside_instance_initializer_block() {
    let fixture = r#"
class C {
    {
        int x;
        /*start*/x = 1;/*end*/
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
    {
        int x;
        x = extracted();
        System.out.println(x);
    }

    private int extracted() {
        int x;
        x = 1;
        return x;
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_return_value_inside_static_initializer_block() {
    let fixture = r#"
class C {
    static {
        int x;
        /*start*/x = 1;/*end*/
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
    static {
        int x;
        x = extracted();
        System.out.println(x);
    }

    private static int extracted() {
        int x;
        x = 1;
        return x;
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
fn extract_method_rejects_reference_to_local_interface_declared_in_enclosing_body() {
    let fixture = r#"
class C {
    void m() {
        interface Local {}
        /*start*/Local x = null;/*end*/
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
fn extract_method_rejects_reference_to_local_annotation_type_declared_in_enclosing_body() {
    let fixture = r#"
class C {
    void m() {
        @interface Local {}
        /*start*/@Local int x = 1;/*end*/
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
fn extract_method_rejects_expression_selection_referencing_local_type_declared_in_enclosing_body() {
    let fixture = r#"
class C {
    void m() {
        class Local {}
        System.out.println(/*start*/new Local()/*end*/);
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
fn extract_method_rejects_reference_to_local_type_in_generic_type_argument() {
    let fixture = r#"
class C {
    void m() {
        class Local {}
        /*start*/java.util.List<Local> xs = null;/*end*/
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
fn extract_method_rejects_expression_selection_referencing_local_type_in_class_literal() {
    let fixture = r#"
class C {
    void m() {
        class Local {}
        System.out.println(/*start*/Local.class/*end*/);
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
fn extract_method_rejects_reference_to_local_type_in_cast_expression() {
    let fixture = r#"
class C {
    void m(Object o) {
        class Local {}
        /*start*/o = (Local) o;/*end*/
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
fn extract_method_rejects_reference_to_local_type_in_instanceof_expression() {
    let fixture = r#"
class C {
    void m(Object o) {
        class Local {}
        /*start*/boolean b = o instanceof Local;/*end*/
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
fn extract_method_rejects_expression_selection_referencing_local_type_in_constructor_reference() {
    let fixture = r#"
class C {
    void m() {
        class Local {}
        java.util.function.Supplier<Local> s = /*start*/Local::new/*end*/;
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
fn extract_method_rejects_expression_selection_referencing_local_type_in_qualified_enum_constant() {
    let fixture = r#"
class C {
    void m() {
        enum Local { A }
        System.out.println(/*start*/Local.A/*end*/);
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
fn extract_method_rejects_reference_to_local_type_in_constructor_body() {
    let fixture = r#"
class C {
    C() {
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
fn extract_method_rejects_reference_to_local_type_in_initializer_block() {
    let fixture = r#"
class C {
    {
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
fn extract_method_rejects_reference_to_local_type_in_static_initializer_block() {
    let fixture = r#"
class C {
    static {
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
fn extract_method_rejects_reference_to_local_type_in_compact_constructor_body() {
    let fixture = r#"
record C(int n) {
    C {
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
fn extract_method_rejects_local_type_declaration() {
    let fixture = r#"
class C {
    void m() {
        /*start*/class Local { }/*end*/
        Local x = new Local();
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
        .expect_err("should reject selection containing a local type declaration");
    assert!(
        err.contains("UnsupportedLocalTypeDeclaration"),
        "unexpected error: {err}"
    );
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
fn extract_method_ignores_checked_exception_thrown_inside_lambda_body() {
    let fixture = r#"
class C {
    void m() {
        /*start*/java.util.concurrent.Callable<Void> c = () -> { throw new java.io.IOException(); };/*end*/
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
        java.util.concurrent.Callable<Void> c = () -> { throw new java.io.IOException(); };
    }
}
"#;

    assert_eq!(actual, expected);
    assert!(
        !actual.contains("throws java.io.IOException"),
        "extracted method must not declare checked exceptions thrown only inside lambda bodies"
    );
}

#[test]
fn extract_method_ignores_return_inside_anonymous_class_body() {
    let fixture = r#"
class C {
    void m() {
        /*start*/new Runnable() { public void run() { return; } };/*end*/
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
        new Runnable() { public void run() { return; } };
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
fn extract_method_rejects_yield_in_expression_selection() {
    let fixture = r#"
class C {
  int m(int x) {
    return /*start*/switch (x) {
      case 0 -> {
        yield 1;
      }
      default -> 2;
    }/*end*/;
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
        .expect_err("should reject yield in expression selection");
    assert!(err.contains("IllegalControlFlow"));
    assert!(err.contains("Yield"));
}

#[test]
fn extract_method_rejects_yield_statement_inside_anonymous_class_body() {
    let fixture = r#"
class C {
  void m(int x) {
    Runnable r = new Runnable() {
      public void run() {
        int y = switch (x) {
          case 0 -> {
            /*start*/yield 1;/*end*/
          }
          default -> 2;
        };
      }
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
        .expect_err("should reject yield inside anonymous class body");
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
fn extract_method_infers_var_throw_type() {
    let fixture = r#"
class C {
    void m() {
        var e = new RuntimeException();
        /*start*/throw e;/*end*/
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
        var e = new RuntimeException();
        boom(e);
    }

    private void boom(RuntimeException e) throws RuntimeException {
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
fn extract_method_adds_throws_for_for_header_var_throw() {
    let fixture = r#"
class C {
    void m(java.io.IOException[] xs) throws java.io.IOException {
        for (java.io.IOException e : xs) {
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
    void m(java.io.IOException[] xs) throws java.io.IOException {
        for (java.io.IOException e : xs) {
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
fn extract_method_preserves_final_for_declared_return_value_when_name_is_shadowed_in_nested_scope()
{
    let fixture = r#"
class C {
    void m(boolean cond) {
        /*start*/if (cond) {
            int x = 1;
            System.out.println(x);
        }
        final int x = 2;/*end*/
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
    void m(boolean cond) {
        final int x = initX(cond);
        System.out.println(x);
    }

    private int initX(boolean cond) {
        if (cond) {
            int x = 1;
            System.out.println(x);
        }
        final int x = 2;
        return x;
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_preserves_annotations_on_declared_return_value() {
    let fixture = r#"
@interface A {}

class C {
    void m() {
        /*start*/@A final int x = 1;/*end*/
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
@interface A {}

class C {
    void m() {
        @A final int x = initX();
        System.out.println(x);
    }

    private int initX() {
        @A final int x = 1;
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
fn extract_method_infers_var_type_as_fully_qualified() {
    // Regression test: typeck's "display" type strings intentionally drop package qualifiers, but
    // Extract Method must emit compilable parameter types even when the original file relied on a
    // fully-qualified initializer (and has no corresponding import).
    let fixture = r#"
class C {
    void m() {
        var xs = new java.util.ArrayList<String>();
        /*start*/System.out.println(xs);/*end*/
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
        var xs = new java.util.ArrayList<String>();
        extracted(xs);
    }

    private void extracted(java.util.ArrayList<java.lang.String> xs) {
        System.out.println(xs);
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

#[test]
fn extract_method_inside_interface_static_method_with_public_visibility_is_static() {
    let fixture = r#"
interface I {
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
        visibility: Visibility::Public,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let edit = refactoring.apply(&source).expect("apply should succeed");
    assert_no_overlaps(&edit);
    let actual = apply_single_file("Main.java", &source, &edit);

    let expected = r#"
interface I {
    static void m(int a) {
        int b = 1;
        extracted(a, b);
        System.out.println("done");
    }

    public static void extracted(int a, int b) {
        System.out.println(a + b);
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_rejects_package_private_visibility_in_interface() {
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
        visibility: Visibility::PackagePrivate,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let err = refactoring
        .apply(&source)
        .expect_err("should reject package-private visibility in interface context");
    assert!(
        err.contains("InvalidVisibilityForInterface"),
        "unexpected error: {err}"
    );
}

#[test]
fn extract_method_inside_interface_generic_method_with_public_visibility_emits_default_before_type_params(
) {
    let fixture = r#"
interface I {
    default <T> void m(T a) {
        /*start*/System.out.println(a);/*end*/
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
    default <T> void m(T a) {
        extracted(a);
        System.out.println("done");
    }

    public default <T> void extracted(T a) {
        System.out.println(a);
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_inc_dec_treats_param_as_read_and_write() {
    let fixture = r#"
class C {
    void m(int x) {
        /*start*/x++;/*end*/
        System.out.println(x);
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
    void m(int x) {
        x = inc(x);
        System.out.println(x);
    }

    private int inc(int x) {
        x++;
        return x;
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_preserves_tab_indentation() {
    let fixture = r#"
class C {
	int m(int a) {
		int b = 1;
		int r;
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
		int r;
		r = compute(a, b);
		return r;
	}

	private int compute(int a, int b) {
		int r;
		r = a + b;
		return r;
	}
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_compound_assignment_treats_param_as_read_and_write() {
    let fixture = r#"
class C {
    void m(int x) {
        /*start*/x += 1;/*end*/
        System.out.println(x);
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
    void m(int x) {
        x = inc(x);
        System.out.println(x);
    }

    private int inc(int x) {
        x += 1;
        return x;
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_method_plain_assignment_does_not_require_uninitialized_local_as_param() {
    let fixture = r#"
class C {
    void m() {
        int x;
        /*start*/x = 1;/*end*/
        System.out.println(x);
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let refactoring = ExtractMethod {
        file: "Main.java".to_string(),
        selection,
        name: "init".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let edit = refactoring.apply(&source).expect("apply should succeed");
    assert_no_overlaps(&edit);
    let actual = apply_single_file("Main.java", &source, &edit);

    let expected = r#"
class C {
    void m() {
        int x;
        x = init();
        System.out.println(x);
    }

    private int init() {
        int x;
        x = 1;
        return x;
    }
}
"#;

    assert_eq!(actual, expected);
}
