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
    assert!(err.contains("IllegalControlFlow"), "unexpected error: {err}");
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
    assert!(err.contains("IllegalControlFlow"), "unexpected error: {err}");
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
