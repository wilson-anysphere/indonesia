use nova_refactor::apply_edits;
use nova_refactor::extract_method::{ExtractMethod, InsertionStrategy, Visibility};
use nova_test_utils::extract_range;
use std::collections::BTreeMap;

fn apply_single_file(file: &str, source: &str, edits: &[nova_refactor::TextEdit]) -> String {
    let mut files = BTreeMap::new();
    files.insert(file.to_string(), source.to_string());
    let out = apply_edits(&files, edits);
    out.get(file).cloned().expect("file must exist")
}

fn assert_no_overlaps(edits: &[nova_refactor::TextEdit]) {
    let mut edit = nova_refactor::WorkspaceEdit::new(
        edits
            .iter()
            .cloned()
            .map(nova_refactor::WorkspaceTextEdit::from)
            .collect(),
    );
    edit.normalize()
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

    let edits = refactoring.apply(&source).expect("apply should succeed");
    assert_no_overlaps(&edits);
    let actual = apply_single_file("Main.java", &source, &edits);

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

    let edits = refactoring.apply(&source).expect("apply should succeed");
    assert_no_overlaps(&edits);
    let actual = apply_single_file("Main.java", &source, &edits);

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

    let edits = refactoring.apply(&source).expect("apply should succeed");
    assert_no_overlaps(&edits);
    let _ = apply_single_file("Main.java", &source, &edits);
}
