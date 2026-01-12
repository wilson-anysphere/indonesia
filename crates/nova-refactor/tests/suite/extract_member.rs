use std::collections::BTreeMap;

use nova_refactor::{
    apply_workspace_edit, extract_constant, extract_field, generate_preview, ExtractError,
    ExtractOptions, FileId, TextDatabase, TextRange,
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
fn extract_constant_infers_long_for_long_literal() {
    let (code, range) = fixture_range(
        r#"
class A {
    void m() {
        long x = /*[*/1L/*]*/;
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
    private static final long VALUE = 1L;

    void m() {
        long x = VALUE;
    }
}
"#
    );
}

#[test]
fn extract_constant_infers_long_for_long_expression() {
    let (code, range) = fixture_range(
        r#"
class A {
    void m() {
        long x = /*[*/1L + 2/*]*/;
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
    private static final long VALUE = 1L + 2;

    void m() {
        long x = VALUE;
    }
}
"#
    );
}

#[test]
fn extract_constant_rejects_selection_with_start_past_eof_without_panicking() {
    let code = "class A { void m() { int x = 1 + 2; } }\n";
    // This range is invalid (start > end) and start is out of bounds. We should return a clean
    // `InvalidSelection` error rather than panicking.
    let range = TextRange::new(code.len() + 5, code.len());

    let err = extract_constant("A.java", code, range, ExtractOptions::default()).unwrap_err();
    assert_eq!(err, ExtractError::InvalidSelection);
}

#[test]
fn extract_constant_infers_double_for_double_literal() {
    let (code, range) = fixture_range(
        r#"
class A {
    void m() {
        double x = /*[*/1.0/*]*/;
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
    private static final double VALUE = 1.0;

    void m() {
        double x = VALUE;
    }
}
"#
    );
}

#[test]
fn extract_constant_infers_double_for_double_expression() {
    let (code, range) = fixture_range(
        r#"
class A {
    void m() {
        double x = /*[*/1.0 + 2/*]*/;
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
    private static final double VALUE = 1.0 + 2;

    void m() {
        double x = VALUE;
    }
}
"#
    );
}

#[test]
fn extract_constant_infers_float_for_float_literal() {
    let (code, range) = fixture_range(
        r#"
class A {
    void m() {
        float x = /*[*/1.0f/*]*/;
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
    private static final float VALUE = 1.0f;

    void m() {
        float x = VALUE;
    }
}
"#
    );
}

#[test]
fn extract_constant_infers_float_for_float_expression() {
    let (code, range) = fixture_range(
        r#"
class A {
    void m() {
        float x = /*[*/1.0f + 2/*]*/;
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
    private static final float VALUE = 1.0f + 2;

    void m() {
        float x = VALUE;
    }
}
"#
    );
}

#[test]
fn extract_constant_infers_long_for_long_cast_expression() {
    let (code, range) = fixture_range(
        r#"
class A {
    void m() {
        long x = /*[*/(long) 1/*]*/;
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
    private static final long VALUE = (long) 1;

    void m() {
        long x = VALUE;
    }
}
"#
    );
}

#[test]
fn extract_constant_infers_double_for_double_cast_expression() {
    let (code, range) = fixture_range(
        r#"
class A {
    void m() {
        double x = /*[*/(double) 1/*]*/;
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
    private static final double VALUE = (double) 1;

    void m() {
        double x = VALUE;
    }
}
"#
    );
}

#[test]
fn extract_constant_infers_int_for_ternary_numeric_expression() {
    let (code, range) = fixture_range(
        r#"
class A {
    void m() {
        int x = /*[*/true ? 1 : 2/*]*/;
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
    private static final int VALUE = true ? 1 : 2;

    void m() {
        int x = VALUE;
    }
}
"#
    );
}

#[test]
fn extract_constant_infers_boolean_for_boolean_literal() {
    let (code, range) = fixture_range(
        r#"
class A {
    void m() {
        boolean x = /*[*/true/*]*/;
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
    private static final boolean VALUE = true;

    void m() {
        boolean x = VALUE;
    }
}
"#
    );
}

#[test]
fn extract_constant_infers_boolean_for_boolean_expression() {
    let (code, range) = fixture_range(
        r#"
class A {
    void m() {
        boolean x = /*[*/true && false/*]*/;
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
    private static final boolean VALUE = true && false;

    void m() {
        boolean x = VALUE;
    }
}
"#
    );
}

#[test]
fn extract_constant_infers_boolean_for_instanceof_expression() {
    let (code, range) = fixture_range(
        r#"
class A {
    void m() {
        boolean x = /*[*/"hi" instanceof String/*]*/;
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
    private static final boolean VALUE = "hi" instanceof String;

    void m() {
        boolean x = VALUE;
    }
}
"#
    );
}

#[test]
fn extract_constant_infers_string_for_text_block_literal() {
    let (code, range) = fixture_range(
        r#"
class A {
    void m() {
        String x = /*[*/"""
hi
"""/*]*/;
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
    private static final String VALUE = """
hi
""";

    void m() {
        String x = VALUE;
    }
}
"#
    );
}

#[test]
fn extract_constant_infers_string_for_string_field_reference() {
    let (code, range) = fixture_range(
        r#"
class A {
    static final String PREFIX = "hi";

    void m() {
        String x = /*[*/A.PREFIX/*]*/;
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
    private static final String VALUE = A.PREFIX;

    static final String PREFIX = "hi";

    void m() {
        String x = VALUE;
    }
}
"#
    );
}

#[test]
fn extract_constant_infers_string_for_string_field_concatenation() {
    let (code, range) = fixture_range(
        r#"
class A {
    static final String PREFIX = "hi";
    static final String SUFFIX = "bye";

    void m() {
        String x = /*[*/A.PREFIX + A.SUFFIX/*]*/;
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
    private static final String VALUE = A.PREFIX + A.SUFFIX;

    static final String PREFIX = "hi";
    static final String SUFFIX = "bye";

    void m() {
        String x = VALUE;
    }
}
"#
    );
}

#[test]
fn extract_constant_infers_long_for_long_field_expression() {
    let (code, range) = fixture_range(
        r#"
class A {
    static final long LEFT = 1L;
    static final long RIGHT = 2L;

    void m() {
        long x = /*[*/A.LEFT + A.RIGHT/*]*/;
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
    private static final long VALUE = A.LEFT + A.RIGHT;

    static final long LEFT = 1L;
    static final long RIGHT = 2L;

    void m() {
        long x = VALUE;
    }
}
"#
    );
}

#[test]
fn extract_constant_infers_boolean_for_boolean_field_bitand() {
    let (code, range) = fixture_range(
        r#"
class A {
    static final boolean LEFT = true;
    static final boolean RIGHT = false;

    void m() {
        boolean x = /*[*/A.LEFT & A.RIGHT/*]*/;
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
    private static final boolean VALUE = A.LEFT & A.RIGHT;

    static final boolean LEFT = true;
    static final boolean RIGHT = false;

    void m() {
        boolean x = VALUE;
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

#[test]
fn extract_constant_generates_preview() {
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

    let db = TextDatabase::new([(FileId::new("A.java"), code)]);
    let preview = generate_preview(&db, &outcome.edit).unwrap();

    assert_eq!(preview.total_files, 1);
    assert_eq!(preview.total_edits, outcome.edit.text_edits.len());
    assert_eq!(
        preview.files[0].modified,
        r#"
class A {
    private static final int VALUE = 1 + 2;

    void m() {
        int x = VALUE;
    }
}
"#
    );
    assert!(preview.files[0]
        .unified_diff
        .contains("private static final int VALUE"));
}

#[test]
fn extract_field_rejects_local_dependency() {
    let (code, range) = fixture_range(
        r#"
class A {
    void m() {
        int x = 1;
        int y = /*[*/x + 1/*]*/;
    }
}
"#,
    );

    let err = extract_field("A.java", &code, range, ExtractOptions::default()).unwrap_err();
    assert_eq!(err, ExtractError::DependsOnLocal);
}

#[test]
fn extract_field_rejects_static_method_context() {
    let (code, range) = fixture_range(
        r#"
class A {
    static int foo = 1;

    static void m() {
        int x = /*[*/foo + 1/*]*/;
    }
}
"#,
    );

    let err = extract_field("A.java", &code, range, ExtractOptions::default()).unwrap_err();
    assert_eq!(err, ExtractError::NotInstanceContext);
}

#[test]
fn extract_constant_rejects_instance_dependency() {
    let (code, range) = fixture_range(
        r#"
class A {
    int foo = 1;

    void m() {
        int x = /*[*/this.foo/*]*/;
    }
}
"#,
    );

    let err = extract_constant("A.java", &code, range, ExtractOptions::default()).unwrap_err();
    assert_eq!(err, ExtractError::NotStaticSafe);
}

#[test]
fn extract_field_allows_instance_dependency() {
    let (code, range) = fixture_range(
        r#"
class A {
    int foo = 1;

    void m() {
        int x = /*[*/this.foo + 1/*]*/;
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
    private final int value = this.foo + 1;

    int foo = 1;

    void m() {
        int x = value;
    }
}
"#
    );
}

#[test]
fn extract_field_qualifies_instance_field_reference() {
    let (code, range) = fixture_range(
        r#"
class A {
    int foo = 1;

    void m() {
        int x = /*[*/foo + 1/*]*/;
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
    private final int value = this.foo + 1;

    int foo = 1;

    void m() {
        int x = value;
    }
}
"#
    );
}

#[test]
fn extract_field_replace_all_does_not_replace_nested_class_occurrences() {
    let (code, range) = fixture_range(
        r#"
class A {
    int foo = 1;

    void m() {
        int x = /*[*/this.foo + 1/*]*/;
        int y = this.foo + 1;
    }

    class Inner {
        int foo = 2;

        void n() {
            int z = this.foo + 1;
        }
    }
}
"#,
    );

    let outcome = extract_field(
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
    private final int value = this.foo + 1;

    int foo = 1;

    void m() {
        int x = value;
        int y = value;
    }

    class Inner {
        int foo = 2;

        void n() {
            int z = this.foo + 1;
        }
    }
}
"#
    );
}

#[test]
fn extract_constant_rejects_increment_expressions() {
    for fixture in [
        r#"
class A {
    void m() {
        int i = 0;
        int x = /*[*/i++/*]*/;
    }
}
"#,
        r#"
class A {
    void m() {
        int i = 0;
        int x = /*[*/++i/*]*/;
    }
}
"#,
    ] {
        let (code, range) = fixture_range(fixture);
        let err = extract_constant("A.java", &code, range, ExtractOptions::default()).unwrap_err();
        assert_eq!(err, ExtractError::SideEffectfulExpression);
    }
}

#[test]
fn extract_constant_qualifies_static_field_reference() {
    let (code, range) = fixture_range(
        r#"
class A {
    static final int BASE = 1;

    void m() {
        int x = /*[*/BASE + 1/*]*/;
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
    private static final int VALUE = A.BASE + 1;

    static final int BASE = 1;

    void m() {
        int x = VALUE;
    }
}
"#
    );
}

#[test]
fn extract_constant_allows_static_member_constant() {
    let (code, range) = fixture_range(
        r#"
class A {
    void m() {
        Object x = /*[*/Math.PI/*]*/;
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
    private static final Object VALUE = Math.PI;

    void m() {
        Object x = VALUE;
    }
}
"#
    );
}

#[test]
fn extract_constant_infers_type_from_enclosing_declaration() {
    let (code, range) = fixture_range(
        r#"
class A {
    void m() {
        double x = /*[*/Math.PI/*]*/;
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
    private static final double VALUE = Math.PI;

    void m() {
        double x = VALUE;
    }
}
"#
    );
}

#[test]
fn extract_constant_infers_long_for_long_expression_without_literals() {
    let (code, range) = fixture_range(
        r#"
class A {
    static long a = 1L;
    static long b = 2L;

    void m() {
        long x = /*[*/a + b/*]*/;
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
    private static final long VALUE = A.a + A.b;

    static long a = 1L;
    static long b = 2L;

    void m() {
        long x = VALUE;
    }
}
"#
    );
}

#[test]
fn extract_field_rejects_lambda_expression() {
    let (code, range) = fixture_range(
        r#"
class A {
    void m() {
        Runnable r = /*[*/() -> { }/*]*/;
    }
}
"#,
    );

    let err = extract_field("A.java", &code, range, ExtractOptions::default()).unwrap_err();
    assert_eq!(err, ExtractError::UnsupportedExpression);
}

#[test]
fn extract_constant_rejects_method_reference_expression() {
    let (code, range) = fixture_range(
        r#"
class A {
    void m() {
        java.util.function.Function<Object, String> f = /*[*/String::valueOf/*]*/;
    }
}
"#,
    );

    let err = extract_constant("A.java", &code, range, ExtractOptions::default()).unwrap_err();
    assert_eq!(err, ExtractError::UnsupportedExpression);
}

#[test]
fn extract_field_rejects_array_initializer_expression() {
    let (code, range) = fixture_range(
        r#"
class A {
    void m() {
        int[] xs = /*[*/{ 1, 2 }/*]*/;
    }
}
"#,
    );

    let err = extract_field("A.java", &code, range, ExtractOptions::default()).unwrap_err();
    assert_eq!(err, ExtractError::UnsupportedExpression);
}

#[test]
fn extract_field_rejects_catch_parameter_dependency() {
    let (code, range) = fixture_range(
        r#"
class A {
    void m() {
        try {
        } catch (Exception e) {
            Object x = /*[*/e/*]*/;
        }
    }
}
"#,
    );

    let err = extract_field("A.java", &code, range, ExtractOptions::default()).unwrap_err();
    assert_eq!(err, ExtractError::DependsOnLocal);
}

#[test]
fn extract_field_rejects_enhanced_for_variable_dependency() {
    let (code, range) = fixture_range(
        r#"
class A {
    void m(java.util.List<String> xs) {
        for (String s : xs) {
            Object x = /*[*/s/*]*/;
        }
    }
}
"#,
    );

    let err = extract_field("A.java", &code, range, ExtractOptions::default()).unwrap_err();
    assert_eq!(err, ExtractError::DependsOnLocal);
}

#[test]
fn extract_field_infers_type_from_enclosing_declaration() {
    let (code, range) = fixture_range(
        r#"
class A {
    int foo = 1;

    void m() {
        int x = /*[*/this.foo/*]*/;
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
    private final int value = this.foo;

    int foo = 1;

    void m() {
        int x = value;
    }
}
"#
    );
}

#[test]
fn extract_field_infers_long_for_long_expression_without_literals() {
    let (code, range) = fixture_range(
        r#"
class A {
    long a = 1L;
    long b = 2L;

    void m() {
        long x = /*[*/a + b/*]*/;
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
    private final long value = this.a + this.b;

    long a = 1L;
    long b = 2L;

    void m() {
        long x = value;
    }
}
"#
    );
}

#[test]
fn extract_field_infers_type_from_return_statement() {
    let (code, range) = fixture_range(
        r#"
class A {
    long a = 1L;
    long b = 2L;

    long m() {
        return /*[*/a + b/*]*/;
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
    private final long value = this.a + this.b;

    long a = 1L;
    long b = 2L;

    long m() {
        return value;
    }
}
"#
    );
}

#[test]
fn extract_field_rejects_method_type_parameter_type() {
    let (code, range) = fixture_range(
        r#"
class A {
    <T> void m() {
        T x = /*[*/null/*]*/;
    }
}
"#,
    );

    let err = extract_field("A.java", &code, range, ExtractOptions::default()).unwrap_err();
    assert_eq!(err, ExtractError::TypeNotInClassScope);
}
