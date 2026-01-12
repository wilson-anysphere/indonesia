use std::collections::BTreeMap;

use nova_refactor::{
    apply_workspace_edit, extract_constant, extract_field, ExtractError, ExtractOptions, FileId,
    TextRange,
};
use pretty_assertions::assert_eq;

fn fixture_range(fixture: &str) -> (String, TextRange) {
    let start_marker = "/*[*/";
    let end_marker = "/*]*/";
    let start = fixture.find(start_marker).expect("missing start marker");
    let mut code = fixture.to_string();
    code.replace_range(start..start + start_marker.len(), "");
    let end = code.find(end_marker).expect("missing end marker");
    let range = TextRange::new(start, end);
    code.replace_range(end..end + end_marker.len(), "");
    (code, range)
}

#[test]
fn extract_constant_inserts_and_replaces() {
    let (code, range) = fixture_range(
        r#"
class A {
    void m() {
        int x = /*[*/1 + 2/*]*/;
    }
}
"#,
    );

    let outcome = extract_constant("A.java", &code, range, ExtractOptions::default()).unwrap();

    let mut files = BTreeMap::new();
    let file_id = FileId::new("A.java");
    files.insert(file_id.clone(), code);
    let updated = apply_workspace_edit(&files, &outcome.edit).expect("apply edits");

    assert_eq!(
        updated.get(&file_id).unwrap(),
         r#"
class A {
    private static final int VALUE = 1 + 2;

    void m() {
        int x = VALUE;
    }
}
"#
    );
}

#[test]
fn extract_constant_replace_all() {
    let (code, range) = fixture_range(
        r#"
class A {
    void m() {
        int x = /*[*/1 + 2/*]*/;
        int y = 1 + 2;
    }
}
"#,
    );

    let outcome = extract_constant(
        "A.java",
        &code,
        range,
        ExtractOptions {
            replace_all: true,
            ..Default::default()
        },
    )
    .unwrap();

    let mut files = BTreeMap::new();
    let file_id = FileId::new("A.java");
    files.insert(file_id.clone(), code);
    let updated = apply_workspace_edit(&files, &outcome.edit).expect("apply edits");

    assert_eq!(
        updated.get(&file_id).unwrap(),
         r#"
class A {
    private static final int VALUE = 1 + 2;

    void m() {
        int x = VALUE;
        int y = VALUE;
    }
}
"#
    );
}

#[test]
fn extract_rejects_side_effects() {
    let (code, range) = fixture_range(
        r#"
class A {
    int foo() { return 1; }
    void m() {
        int x = /*[*/foo()/*]*/;
    }
}
"#,
    );

    let err = extract_constant("A.java", &code, range, ExtractOptions::default()).unwrap_err();
    assert_eq!(err, ExtractError::SideEffectfulExpression);
}

#[test]
fn extract_field_inserts_and_replaces() {
    let (code, range) = fixture_range(
        r#"
class A {
    void m() {
        int x = /*[*/1 + 2/*]*/;
    }
}
"#,
    );

    let outcome = extract_field("A.java", &code, range, ExtractOptions::default()).unwrap();

    let mut files = BTreeMap::new();
    let file_id = FileId::new("A.java");
    files.insert(file_id.clone(), code);
    let updated = apply_workspace_edit(&files, &outcome.edit).expect("apply edits");

    assert_eq!(
        updated.get(&file_id).unwrap(),
         r#"
class A {
    private final int value = 1 + 2;

    void m() {
        int x = value;
    }
}
"#
    );
}
