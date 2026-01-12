use nova_refactor::extract_method::{ExtractMethod, InsertionStrategy, Visibility};
use nova_refactor::{apply_workspace_edit, FileId, WorkspaceEdit};
use nova_syntax::parse_java;
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
    assert!(err.contains("IllegalControlFlow"));
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
fn extract_method_preserves_trailing_line_comment_after_statement() {
    let fixture = r#"
class C {
    void m(int a) {
        /*start*/System.out.println(a);/*end*/ // trailing
        System.out.println("next");
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
    let actual = apply_single_file("Main.java", &source, &edit);

    let expected = r#"
class C {
    void m(int a) {
        log(a); // trailing
        System.out.println("next");
    }

    private void log(int a) {
        System.out.println(a);
    }
}
"#;

    assert_eq!(actual, expected);

    let parsed = parse_java(&actual);
    assert!(
        parsed.errors.is_empty(),
        "expected extracted code to parse without errors, got: {:?}",
        parsed.errors
    );
}

#[test]
fn extract_method_preserves_adjacent_block_comment_after_statement() {
    let fixture = r#"
class C {
    void m(int a) {
        /*start*/System.out.println(a);/*end*//*trailing*/
        System.out.println("next");
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
    let actual = apply_single_file("Main.java", &source, &edit);

    let expected = r#"
class C {
    void m(int a) {
        log(a);/*trailing*/
        System.out.println("next");
    }

    private void log(int a) {
        System.out.println(a);
    }
}
"#;

    assert_eq!(actual, expected);

    let parsed = parse_java(&actual);
    assert!(
        parsed.errors.is_empty(),
        "expected extracted code to parse without errors, got: {:?}",
        parsed.errors
    );
}

#[test]
fn extract_method_preserves_newline_when_selection_ends_at_line_boundary() {
    // Selection includes the terminating newline; ensure replacement keeps it so that the
    // following comment stays on its own line.
    let fixture = r#"
class C {
    void m(int a) {
        /*start*/System.out.println(a);
/*end*/        // trailing
        System.out.println("next");
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
    let actual = apply_single_file("Main.java", &source, &edit);

    let expected = r#"
class C {
    void m(int a) {
        log(a);
        // trailing
        System.out.println("next");
    }

    private void log(int a) {
        System.out.println(a);
    }
}
"#;

    assert_eq!(actual, expected);

    let parsed = parse_java(&actual);
    assert!(
        parsed.errors.is_empty(),
        "expected extracted code to parse without errors, got: {:?}",
        parsed.errors
    );
}
