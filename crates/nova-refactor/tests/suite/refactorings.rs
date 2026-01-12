use nova_refactor::{
    apply_text_edits, apply_workspace_edit, extract_variable, inline_variable, materialize, rename,
    Conflict, ExtractVariableParams, FileId, InlineVariableParams, JavaSymbolKind,
    RefactorDatabase, RefactorJavaDatabase, Reference, ReferenceKind, RenameParams, SemanticChange,
    SemanticRefactorError, SymbolDefinition, SymbolId, TextDatabase, WorkspaceTextRange,
};
use nova_test_utils::extract_range;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;

fn strip_selection_markers(src: &str) -> (String, WorkspaceTextRange) {
    let start_marker = "/*select*/";
    let end_marker = "/*end*/";
    let start = src.find(start_marker).expect("start marker");
    let end = src.find(end_marker).expect("end marker");
    assert!(
        start < end,
        "expected start marker to come before end marker"
    );

    let selection_start = start;
    let selection_len = end - (start + start_marker.len());
    let selection_end = selection_start + selection_len;

    let mut cleaned = String::new();
    cleaned.push_str(&src[..start]);
    cleaned.push_str(&src[start + start_marker.len()..end]);
    cleaned.push_str(&src[end + end_marker.len()..]);

    (
        cleaned,
        WorkspaceTextRange::new(selection_start, selection_end),
    )
}

fn to_crlf(text: &str) -> String {
    // The fixtures in this file are written with `\n` newlines. Convert to CRLF so we can assert
    // refactorings preserve the file's existing newline style.
    text.replace('\n', "\r\n")
}

fn assert_all_newlines_are_crlf(text: &str) {
    let bytes = text.as_bytes();
    for (idx, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            assert!(
                idx > 0 && bytes[idx - 1] == b'\r',
                "found LF not preceded by CR at byte offset {idx}"
            );
        }
        if b == b'\r' {
            assert!(
                idx + 1 < bytes.len() && bytes[idx + 1] == b'\n',
                "found stray CR not followed by LF at byte offset {idx}"
            );
        }
    }
}

const EXTRACT_VARIABLE_EVAL_ORDER_GUARD_REASON: &str =
    "cannot extract because it may change evaluation order";

#[test]
fn rename_updates_all_occurrences_not_strings() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int foo = 1;
    System.out.println(foo);
    String s = "foo";
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int foo").unwrap() + "int ".len() + 1;
    let symbol = db.symbol_at(&file, offset).expect("symbol at foo");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "bar".into(),
        },
    )
    .unwrap();
    let after = apply_text_edits(src, &edit.text_edits).unwrap();

    assert!(after.contains("int bar = 1;"));
    assert!(after.contains("println(bar);"));
    assert!(after.contains("\"foo\""));
    assert!(!after.contains("\"bar\""));
}

#[test]
fn rename_updates_occurrences_in_assert_statement() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int foo = 1;
    assert foo > 0 : foo;
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int foo").unwrap() + "int ".len() + 1;
    let symbol = db.symbol_at(&file, offset).expect("symbol at foo");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "bar".into(),
        },
    )
    .unwrap();
    let after = apply_text_edits(src, &edit.text_edits).unwrap();

    assert!(after.contains("int bar = 1;"));
    assert!(after.contains("assert bar > 0 : bar;"), "got: {after}");
    assert!(
        !after.contains("foo"),
        "expected all occurrences to be renamed: {after}"
    );
}

#[test]
fn symbol_at_package_decl_returns_package_kind() {
    let file = FileId::new("C.java");
    let src = "package com.example; class C {}";

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("com.example").unwrap() + 1;
    let symbol = db.symbol_at(&file, offset).expect("symbol at package name");

    assert_eq!(db.symbol_kind(symbol), Some(JavaSymbolKind::Package));
}

#[test]
fn rename_conflict_detection_triggers_on_collision() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int foo = 1;
    int bar = 2;
    System.out.println(foo + bar);
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int foo").unwrap() + "int ".len() + 1;
    let symbol = db.symbol_at(&file, offset).expect("symbol at foo");

    let err = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "bar".into(),
        },
    )
    .unwrap_err();
    let SemanticRefactorError::Conflicts(conflicts) = err else {
        panic!("expected conflicts, got: {err:?}");
    };

    assert!(
        conflicts
            .iter()
            .any(|c| matches!(c, Conflict::NameCollision { name, .. } if name == "bar")),
        "expected NameCollision conflict: {conflicts:?}"
    );
}

#[test]
fn rename_for_init_variable_does_not_conflict_with_later_block_local() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    for (int foo = 0; foo < 1; foo++) {
    }
    int bar = 0;
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int foo").unwrap() + "int ".len() + 1;
    let symbol = db.symbol_at(&file, offset).expect("symbol at foo");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "bar".into(),
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();

    assert!(
        after.contains("for (int bar = 0; bar < 1; bar++)"),
        "expected for-loop binding to be renamed: {after}"
    );
    assert!(
        after.contains("}\n    int bar = 0;\n"),
        "expected later bar declaration to remain (scopes do not overlap): {after}"
    );
    assert!(
        !after.contains("foo"),
        "expected all occurrences of foo to be renamed: {after}"
    );
}

#[test]
fn rename_for_init_multi_declarator_variable() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    for (int a = 1, b = 2; a < b; a++) {
      System.out.println(b);
    }
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("b =").unwrap();
    let symbol = db.symbol_at(&file, offset).expect("symbol at b");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "c".into(),
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    let expected = r#"class Test {
  void m() {
    for (int a = 1, c = 2; a < c; a++) {
      System.out.println(c);
    }
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_generates_valid_edit() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int x = 1 + 2;
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let expr_start = src.find("1 + 2").unwrap();
    let expr_end = expr_start + "1 + 2".len();

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(expr_start, expr_end),
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    let expected = r#"class Test {
  void m() {
    var sum = 1 + 2;
    int x = sum;
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_rejects_selection_end_past_eof_without_panicking() {
    let file = FileId::new("Test.java");
    let src = "class Test { void m() { int x = 1 + 2; } }\n";

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(0, src.len() + 5),
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    // We return a normal error rather than panicking on the invalid range.
    assert!(matches!(err, SemanticRefactorError::Edit(_)));
}

#[test]
fn extract_variable_preserves_crlf_newlines() {
    let file = FileId::new("Test.java");
    let src_lf = r#"class Test {
  void m() {
    int x = 1 + 2;
  }
}
"#;
    let src = to_crlf(src_lf);

    let db = RefactorJavaDatabase::new([(file.clone(), src.clone())]);
    let expr_start = src.find("1 + 2").unwrap();
    let expr_end = expr_start + "1 + 2".len();

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(expr_start, expr_end),
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    assert_all_newlines_are_crlf(&after);
    assert!(
        after.contains("    var sum = 1 + 2;\r\n"),
        "expected inserted declaration to be indented correctly: {after:?}"
    );

    let expected_lf = r#"class Test {
  void m() {
    var sum = 1 + 2;
    int x = sum;
  }
}
"#;
    let expected = to_crlf(expected_lf);
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_preserves_no_final_newline() {
    let file = FileId::new("Test.java");
    let src = "class Test {\n  void m() {\n    int x = 1 + 2;\n  }\n}";
    assert!(
        !src.ends_with('\n') && !src.ends_with('\r'),
        "test precondition: fixture must not end with a newline"
    );

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let expr_start = src.find("1 + 2").unwrap();
    let expr_end = expr_start + "1 + 2".len();

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(expr_start, expr_end),
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    assert!(
        !after.ends_with('\n') && !after.ends_with('\r'),
        "expected refactoring to preserve lack of final newline, got: {after:?}"
    );
    let expected = "class Test {\n  void m() {\n    var sum = 1 + 2;\n    int x = sum;\n  }\n}";
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_replaces_whole_expression_statement() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    1 + 2;
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let expr_start = src.find("1 + 2").unwrap();
    let expr_end = expr_start + "1 + 2".len();

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(expr_start, expr_end),
            name: "result".into(),
            use_var: false,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    let expected = r#"class Test {
  void m() {
    int result = 1 + 2;
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_replaces_whole_expression_statement_preserving_inline_comments() {
    let file = FileId::new("Test.java");
    let fixture = r#"class Test {
  void m() {
    /*leading*/
    /*select*/1 + 2 /*middle*//*end*/;
  }
}
"#;

    let (src, expr_range) = strip_selection_markers(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.clone())]);

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "result".into(),
            use_var: false,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    let expected = r#"class Test {
  void m() {
    /*leading*/
    int result = 1 + 2 /*middle*/;
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_replaces_whole_expression_statement_with_trailing_comment_when_selection_excludes_comment(
) {
    let file = FileId::new("Test.java");
    let fixture = r#"class Test {
  void m() {
    /*select*/1 + 2/*end*/ /*middle*/;
  }
}
"#;

    let (src, expr_range) = strip_selection_markers(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.clone())]);

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "result".into(),
            use_var: false,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    let expected = r#"class Test {
  void m() {
    int result = 1 + 2 /*middle*/;
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_preserves_trailing_comment_when_selection_excludes_comment() {
    let file = FileId::new("Test.java");
    let fixture = r#"class Foo {}

class Test {
  Foo m() {
    return /*select*/new Foo()/*end*/ /*middle*/;
  }
}
"#;

    let (src, expr_range) = strip_selection_markers(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.clone())]);

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "result".into(),
            use_var: false,
            replace_all: true,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    let expected = r#"class Foo {}

class Test {
  Foo m() {
    Foo result = new Foo();
    return result /*middle*/;
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_splits_multi_declarator_local_declaration() {
    let file = FileId::new("Test.java");
    let fixture = r#"class Test {
  void m() {
    int a = 1, b = /*select*/a + 2/*end*/;
  }
}
"#;

    let (src, expr_range) = strip_selection_markers(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.clone())]);

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "tmp".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    let expected = r#"class Test {
  void m() {
    int a = 1;
    var tmp = a + 2;
    int b = tmp;
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_splits_multi_declarator_with_line_comment_between_declarators() {
    let file = FileId::new("Test.java");
    let fixture = r#"class Test {
  void m() {
    int a = 1, // comment about b
        b = /*select*/a + 2/*end*/;
  }
}
"#;

    let (src, expr_range) = strip_selection_markers(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.clone())]);

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "tmp".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    let expected = r#"class Test {
  void m() {
    int a = 1; // comment about b
    var tmp = a + 2;
    int b = tmp;
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_replace_all_does_not_cross_lambda_boundary() {
    let file = FileId::new("Test.java");
    let fixture = r#"class Test {
  void m() {
    int x = 0;
    int a = /*select*/1 / x/*end*/;
    Runnable r = () -> System.out.println(1 / x);
    int b = 1 / x;
  }
}
"#;

    let (src, expr_range) = strip_selection_markers(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.clone())]);

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "div".into(),
            use_var: true,
            replace_all: true,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    let expected = r#"class Test {
  void m() {
    int x = 0;
    var div = 1 / x;
    int a = div;
    Runnable r = () -> System.out.println(1 / x);
    int b = div;
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_replace_all_does_not_cross_anonymous_class_boundary() {
    let file = FileId::new("Test.java");
    let fixture = r#"class Test {
  void m() {
    int x = 0;
    int a = /*select*/1 / x/*end*/;
    Runnable r = new Runnable() {
      @Override public void run() {
        System.out.println(1 / x);
      }
    };
    int b = 1 / x;
  }
}
"#;

    let (src, expr_range) = strip_selection_markers(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.clone())]);

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "div".into(),
            use_var: true,
            replace_all: true,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    let expected = r#"class Test {
  void m() {
    int x = 0;
    var div = 1 / x;
    int a = div;
    Runnable r = new Runnable() {
      @Override public void run() {
        System.out.println(1 / x);
      }
    };
    int b = div;
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_replace_all_does_not_replace_occurrences_outside_switch_block() {
    let file = FileId::new("Test.java");
    let fixture = r#"class Test {
  void m(int x) {
    switch (x) {
      case 1:
        System.out.println(/*select*/1 + 2/*end*/);
        break;
    }
    int y = 1 + 2;
  }
}
"#;

    let (src, expr_range) = strip_selection_markers(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.clone())]);

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "sum".into(),
            use_var: true,
            replace_all: true,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    let expected = r#"class Test {
  void m(int x) {
    switch (x) {
      case 1:
        var sum = 1 + 2;
        System.out.println(sum);
        break;
    }
    int y = 1 + 2;
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_replace_all_does_not_cross_switch_case_group_boundary() {
    let file = FileId::new("Test.java");
    let fixture = r#"class Test {
  void m(int x) {
    switch (x) {
      case 1:
        int a = /*select*/1 / x/*end*/;
        int b = 1 / x;
        break;
      case 2:
        int c = 1 / x;
        break;
    }
    int d = 1 / x;
  }
}
"#;

    let (src, expr_range) = strip_selection_markers(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.clone())]);

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "div".into(),
            use_var: true,
            replace_all: true,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    let expected = r#"class Test {
  void m(int x) {
    switch (x) {
      case 1:
        var div = 1 / x;
        int a = div;
        int b = div;
        break;
      case 2:
        int c = 1 / x;
        break;
    }
    int d = 1 / x;
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_rejects_name_that_would_shadow_field_used_later_unqualified() {
    let file = FileId::new("Test.java");
    let fixture = r#"class Test {
  int value = 0;

  void m() {
    int x = /*select*/1 + 2/*end*/;
    System.out.println(value);
  }
}
"#;

    let (src, expr_range) = strip_selection_markers(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.clone())]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file,
            expr_range,
            name: "value".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    let SemanticRefactorError::Conflicts(conflicts) = err else {
        panic!("expected conflicts, got: {err:?}");
    };

    assert!(
        conflicts
            .iter()
            .any(|c| matches!(c, Conflict::FieldShadowing { name, .. } if name == "value")),
        "expected FieldShadowing conflict: {conflicts:?}"
    );
}

#[test]
fn extract_variable_allows_name_that_matches_field_when_later_access_is_qualified() {
    let file = FileId::new("Test.java");
    let fixture = r#"class Test {
  int value = 0;

  void m() {
    int x = /*select*/1 + 2/*end*/;
    System.out.println(this.value);
  }
}
"#;

    let (src, expr_range) = strip_selection_markers(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.clone())]);

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file,
            expr_range,
            name: "value".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    let expected = r#"class Test {
  int value = 0;

  void m() {
    var value = 1 + 2;
    int x = value;
    System.out.println(this.value);
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_allows_name_that_matches_field_when_later_access_is_qualified_name() {
    let file = FileId::new("Test.java");
    let fixture = r#"class Test {
  static int value = 0;

  void m() {
    int x = /*select*/1 + 2/*end*/;
    System.out.println(Test.value);
  }
}
"#;

    let (src, expr_range) = strip_selection_markers(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.clone())]);

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "value".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    let expected = r#"class Test {
  static int value = 0;

  void m() {
    var value = 1 + 2;
    int x = value;
    System.out.println(Test.value);
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_rejects_name_that_would_shadow_field_used_as_name_qualifier() {
    let file = FileId::new("Test.java");
    let fixture = r#"class Test {
  static class Box { int field = 0; }

  Box value = new Box();

  void m() {
    int x = /*select*/1 + 2/*end*/;
    System.out.println(value.field);
  }
}
"#;

    let (src, expr_range) = strip_selection_markers(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.clone())]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file,
            expr_range,
            name: "value".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    let SemanticRefactorError::Conflicts(conflicts) = err else {
        panic!("expected conflicts, got: {err:?}");
    };

    assert!(
        conflicts
            .iter()
            .any(|c| matches!(c, Conflict::FieldShadowing { name, .. } if name == "value")),
        "expected FieldShadowing conflict: {conflicts:?}"
    );
}

#[test]
fn extract_variable_allows_name_that_matches_field_when_later_access_is_this_qualified_chain() {
    let file = FileId::new("Test.java");
    let fixture = r#"class Test {
  static class Box { int field = 0; }

  Box value = new Box();

  void m() {
    int x = /*select*/1 + 2/*end*/;
    System.out.println(this.value.field);
  }
}
"#;

    let (src, expr_range) = strip_selection_markers(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.clone())]);

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "value".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    let expected = r#"class Test {
  static class Box { int field = 0; }

  Box value = new Box();

  void m() {
    var value = 1 + 2;
    int x = value;
    System.out.println(this.value.field);
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_rejects_name_that_would_shadow_outer_field_used_in_inner_class() {
    let file = FileId::new("Outer.java");
    let fixture = r#"class Outer {
  int value = 0;

  class Inner {
    void m() {
      int x = /*select*/1 + 2/*end*/;
      System.out.println(value);
    }
  }
}
"#;

    let (src, expr_range) = strip_selection_markers(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.clone())]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file,
            expr_range,
            name: "value".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    let SemanticRefactorError::Conflicts(conflicts) = err else {
        panic!("expected conflicts, got: {err:?}");
    };

    assert!(
        conflicts
            .iter()
            .any(|c| matches!(c, Conflict::FieldShadowing { name, .. } if name == "value")),
        "expected FieldShadowing conflict: {conflicts:?}"
    );
}

#[test]
fn extract_variable_allows_name_that_matches_outer_field_when_access_is_outer_this_qualified() {
    let file = FileId::new("Outer.java");
    let fixture = r#"class Outer {
  int value = 0;

  class Inner {
    void m() {
      int x = /*select*/1 + 2/*end*/;
      System.out.println(Outer.this.value);
    }
  }
}
"#;

    let (src, expr_range) = strip_selection_markers(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.clone())]);

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "value".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    let expected = r#"class Outer {
  int value = 0;

  class Inner {
    void m() {
      var value = 1 + 2;
      int x = value;
      System.out.println(Outer.this.value);
    }
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_allows_name_that_matches_field_when_replace_all_replaces_later_unqualified_uses() {
    let file = FileId::new("Test.java");
    let fixture = r#"class Test {
  int value = 0;

  void m() {
    System.out.println(/*select*/value/*end*/);
    System.out.println(value);
  }
}
"#;

    let (src, expr_range) = strip_selection_markers(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.clone())]);

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "value".into(),
            use_var: true,
            replace_all: true,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    let expected = r#"class Test {
  int value = 0;

  void m() {
    var value = value;
    System.out.println(value);
    System.out.println(value);
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_rejects_name_that_would_shadow_field_used_later_unqualified_in_switch_case_group(
) {
    let file = FileId::new("Test.java");
    let fixture = r#"class Test {
  int value = 0;

  void m(int x) {
    switch (x) {
      case 1:
        int y = /*select*/1 + 2/*end*/;
        System.out.println(value);
        break;
    }
  }
}
"#;

    let (src, expr_range) = strip_selection_markers(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src)]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file,
            expr_range,
            name: "value".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    let SemanticRefactorError::Conflicts(conflicts) = err else {
        panic!("expected conflicts, got: {err:?}");
    };

    assert!(
        conflicts
            .iter()
            .any(|c| matches!(c, Conflict::FieldShadowing { name, .. } if name == "value")),
        "expected FieldShadowing conflict: {conflicts:?}"
    );
}

#[test]
fn extract_variable_rejects_name_that_would_shadow_field_used_later_unqualified_in_other_switch_case_group(
) {
    let file = FileId::new("Test.java");
    let fixture = r#"class Test {
  int value = 0;

  void m(int x) {
    switch (x) {
      case 1:
        int y = /*select*/1 + 2/*end*/;
        break;
      case 2:
        System.out.println(value);
        break;
    }
  }
}
"#;

    let (src, expr_range) = strip_selection_markers(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src)]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file,
            expr_range,
            name: "value".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    let SemanticRefactorError::Conflicts(conflicts) = err else {
        panic!("expected conflicts, got: {err:?}");
    };

    assert!(
        conflicts
            .iter()
            .any(|c| matches!(c, Conflict::FieldShadowing { name, .. } if name == "value")),
        "expected FieldShadowing conflict: {conflicts:?}"
    );
}

#[test]
fn extract_variable_allows_name_that_matches_field_in_braced_switch_case_group() {
    let file = FileId::new("Test.java");
    let fixture = r#"class Test {
  int value = 0;

  void m(int x) {
    switch (x) {
      case 1: {
        int y = /*select*/1 + 2/*end*/;
        break;
      }
      case 2:
        System.out.println(value);
        break;
    }
  }
}
"#;

    let (src, expr_range) = strip_selection_markers(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.clone())]);

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file,
            expr_range,
            name: "value".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    let expected = r#"class Test {
  int value = 0;

  void m(int x) {
    switch (x) {
      case 1: {
        var value = 1 + 2;
        int y = value;
        break;
      }
      case 2:
        System.out.println(value);
        break;
    }
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_rejects_dependency_written_earlier_in_same_statement() {
    let file = FileId::new("Test.java");
    let fixture = r#"class Test {
  void m(int x) {
    foo(x = 1, /*select*/x/*end*/);
  }

  void foo(int a, int b) {}
}
"#;

    let (src, expr_range) = strip_selection_markers(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src)]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "tmp".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(
            err,
            SemanticRefactorError::ExtractNotSupported { reason }
                if reason
                    == "cannot extract expression that depends on a variable written earlier in the same statement"
        ),
        "expected ExtractNotSupported, got: {err:?}"
    );
}

#[test]
fn extract_variable_rejects_qualified_dependency_written_earlier_in_same_statement() {
    let file = FileId::new("Test.java");
    let fixture = r#"class Test {
  static class Box { int value; }

  void m(Box x) {
    foo(x = new Box(), /*select*/x.value/*end*/);
  }

  void foo(Box a, int b) {}
}
"#;

    let (src, expr_range) = strip_selection_markers(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src)]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "tmp".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(
            err,
            SemanticRefactorError::ExtractNotSupported { reason }
                if reason
                    == "cannot extract expression that depends on a variable written earlier in the same statement"
        ),
        "expected ExtractNotSupported, got: {err:?}"
    );
}

#[test]
fn extract_variable_allows_selection_inside_assignment_rhs() {
    let file = FileId::new("Test.java");
    let fixture = r#"class Test {
  int x;

  int foo(int a) { return a; }

  void m() {
    x = foo(/*select*/x/*end*/);
  }
}
"#;

    let (src, expr_range) = strip_selection_markers(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.clone())]);

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "tmp".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    let expected = r#"class Test {
  int x;

  int foo(int a) { return a; }

  void m() {
    var tmp = x;
    x = foo(tmp);
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_rejects_instanceof_pattern_expression() {
    let file = FileId::new("Test.java");
    let fixture = r#"class Test {
  void m(Object obj) {
    if (/*select*/obj instanceof String s/*end*/ && s.length() > 0) {
      System.out.println(s);
    }
  }
}
"#;

    let (src, expr_range) = strip_selection_markers(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.clone())]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "tmp".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(err, SemanticRefactorError::ExtractNotSupported { .. }),
        "expected ExtractNotSupported, got: {err:?}"
    );
    assert_eq!(
        err.to_string(),
        "extract variable is not supported in this context: cannot extract `instanceof` pattern matching expression"
    );
}

#[test]
fn extract_variable_replace_all_ignores_equivalent_occurrences_before_insertion_stmt() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int a = 0;
    int b = a + 1;
    System.out.println(a + 1);
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let expr_start = src.rfind("a + 1").unwrap();
    let expr_end = expr_start + "a + 1".len();

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(expr_start, expr_end),
            name: "sum".into(),
            use_var: true,
            replace_all: true,
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    let expected = r#"class Test {
  void m() {
    int a = 0;
    int b = a + 1;
    var sum = a + 1;
    System.out.println(sum);
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_replace_all_replaces_occurrences_after_insertion_stmt_in_same_block() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int a = 0;
    System.out.println(a + 1);
    int b = a + 1;
    int c = a + 1;
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let expr_start = src.find("a + 1").unwrap();
    let expr_end = expr_start + "a + 1".len();

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(expr_start, expr_end),
            name: "sum".into(),
            use_var: true,
            replace_all: true,
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    let expected = r#"class Test {
  void m() {
    int a = 0;
    var sum = a + 1;
    System.out.println(sum);
    int b = sum;
    int c = sum;
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_rejects_name_conflict_with_instanceof_pattern_variable() {
    let fixture = r#"class C {
  void m(Object o) {
    if (o instanceof String s) {
      System.out.println(/*start*/1 + 2/*end*/);
    }
  }
}
"#;
    let (src, expr_range) = extract_range(fixture);
    let file = FileId::new("Test.java");
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "s".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    let SemanticRefactorError::Conflicts(conflicts) = err else {
        panic!("expected conflicts, got: {err:?}");
    };

    assert!(
        conflicts
            .iter()
            .any(|c| matches!(c, Conflict::NameCollision { name, .. } if name == "s")),
        "expected NameCollision conflict: {conflicts:?}"
    );
}

#[test]
fn extract_variable_rejects_name_conflict_with_switch_pattern_variable() {
    let fixture = r#"class C {
  void m(Object o) {
    switch (o) {
      case String s -> {
        System.out.println(/*start*/1 + 2/*end*/);
      }
      default -> {}
    }
  }
}
"#;
    let (src, expr_range) = extract_range(fixture);
    let file = FileId::new("Test.java");
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "s".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    let SemanticRefactorError::Conflicts(conflicts) = err else {
        panic!("expected conflicts, got: {err:?}");
    };

    assert!(
        conflicts
            .iter()
            .any(|c| matches!(c, Conflict::NameCollision { name, .. } if name == "s")),
        "expected NameCollision conflict: {conflicts:?}"
    );
}

#[test]
fn extract_variable_allows_name_reuse_in_other_switch_rule_than_pattern_variable() {
    let fixture = r#"class C {
  void m(Object o) {
    switch (o) {
      case String s -> {
        System.out.println(s);
      }
      default -> {
        System.out.println(/*start*/1 + 2/*end*/);
      }
    }
  }
}
"#;
    let (src, expr_range) = extract_range(fixture);
    let file = FileId::new("Test.java");
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "s".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    let expected = r#"class C {
  void m(Object o) {
    switch (o) {
      case String s -> {
        System.out.println(s);
      }
      default -> {
        var s = 1 + 2;
        System.out.println(s);
      }
    }
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_rejects_name_conflict_with_try_with_resources_resource_variable() {
    let fixture = r#"class C {
  void m(java.io.InputStream src) throws Exception {
    try (java.io.InputStream in = src) {
      System.out.println(/*start*/1 + 2/*end*/);
    }
  }
}
"#;
    let (src, expr_range) = extract_range(fixture);
    let file = FileId::new("Test.java");
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "in".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    let SemanticRefactorError::Conflicts(conflicts) = err else {
        panic!("expected conflicts, got: {err:?}");
    };

    assert!(
        conflicts
            .iter()
            .any(|c| matches!(c, Conflict::NameCollision { name, .. } if name == "in")),
        "expected NameCollision conflict: {conflicts:?}"
    );
}

#[test]
fn extract_variable_rejects_empty_name() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int x = 1 + 2;
  }
 }
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let expr_start = src.find("1 + 2").unwrap();
    let expr_end = expr_start + "1 + 2".len();

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(expr_start, expr_end),
            name: "".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(err, SemanticRefactorError::InvalidIdentifier { .. }),
        "expected invalid identifier error, got: {err:?}"
    );
    assert_eq!(
        err.to_string(),
        "invalid variable name `<empty>`: name is empty (after trimming whitespace)"
    );
}

#[test]
fn extract_variable_conflicts_with_inner_block_local() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int x = 1 + 2;
    {
      int sum = 0;
      System.out.println(sum);
    }
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let expr_start = src.find("1 + 2").unwrap();
    let expr_end = expr_start + "1 + 2".len();

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(expr_start, expr_end),
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    let SemanticRefactorError::Conflicts(conflicts) = err else {
        panic!("expected conflicts, got: {err:?}");
    };

    assert!(
        conflicts
            .iter()
            .any(|c| matches!(c, Conflict::NameCollision { name, .. } if name == "sum")),
        "expected NameCollision conflict: {conflicts:?}"
    );
}

#[test]
fn extract_variable_conflicts_with_lambda_parameter() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int x = 1 + 2;
    java.util.function.IntUnaryOperator f = (sum) -> sum + 1;
    System.out.println(f.applyAsInt(x));
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let expr_start = src.find("1 + 2").unwrap();
    let expr_end = expr_start + "1 + 2".len();

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(expr_start, expr_end),
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    let SemanticRefactorError::Conflicts(conflicts) = err else {
        panic!("expected conflicts, got: {err:?}");
    };

    assert!(
        conflicts
            .iter()
            .any(|c| matches!(c, Conflict::NameCollision { name, .. } if name == "sum")),
        "expected NameCollision conflict: {conflicts:?}"
    );
}

#[test]
fn extract_variable_allows_try_resource_shadowing_in_catch_clause() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    try (java.io.ByteArrayInputStream sum = new java.io.ByteArrayInputStream(new byte[0])) {
      System.out.println(sum);
    } catch (RuntimeException e) {
      int x = 1 + 2;
      System.out.println(x);
    }
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let expr_start = src.find("1 + 2").unwrap();
    let expr_end = expr_start + "1 + 2".len();

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(expr_start, expr_end),
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    let expected = r#"class Test {
  void m() {
    try (java.io.ByteArrayInputStream sum = new java.io.ByteArrayInputStream(new byte[0])) {
      System.out.println(sum);
    } catch (RuntimeException e) {
      var sum = 1 + 2;
      int x = sum;
      System.out.println(x);
    }
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_allows_name_collision_with_switch_pattern_in_other_case_group() {
    let file = FileId::new("Test.java");
    let fixture = r#"class Test {
  void m(Object o) {
    switch (o) {
      case String s:
        System.out.println(s);
        break;
      default:
        System.out.println(/*select*/1 + 2/*end*/);
        break;
    }
  }
}
"#;

    let (src, expr_range) = strip_selection_markers(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.clone())]);

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "s".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    let expected = r#"class Test {
  void m(Object o) {
    switch (o) {
      case String s:
        System.out.println(s);
        break;
      default:
        var s = 1 + 2;
        System.out.println(s);
        break;
    }
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_rejects_name_starting_with_digit() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int x = 1 + 2;
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let expr_start = src.find("1 + 2").unwrap();
    let expr_end = expr_start + "1 + 2".len();

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(expr_start, expr_end),
            name: "1x".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(err, SemanticRefactorError::InvalidIdentifier { .. }),
        "expected invalid identifier error, got: {err:?}"
    );
    assert_eq!(
        err.to_string(),
        "invalid variable name `1x`: must start with '_' or an ASCII letter"
    );
}

#[test]
fn extract_variable_rejects_keyword_name() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int x = 1 + 2;
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let expr_start = src.find("1 + 2").unwrap();
    let expr_end = expr_start + "1 + 2".len();

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(expr_start, expr_end),
            name: "class".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(err, SemanticRefactorError::InvalidIdentifier { .. }),
        "expected invalid identifier error, got: {err:?}"
    );
    assert_eq!(
        err.to_string(),
        "invalid variable name `class`: is a reserved Java keyword"
    );
}

#[test]
fn extract_variable_rejects_var_for_null_initializer() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    String x = null;
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let expr_start = src.find("null").unwrap();
    let expr_end = expr_start + "null".len();

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(expr_start, expr_end),
            name: "extracted".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(matches!(
        err,
        SemanticRefactorError::VarNotAllowedForInitializer
    ));
}

#[test]
fn extract_variable_rejects_var_for_lambda_initializer() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  interface IntSupplier {
    int getAsInt();
  }

  void m() {
    IntSupplier s = () -> 1;
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let expr_start = src.find("() -> 1").unwrap();
    let expr_end = expr_start + "() -> 1".len();

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(expr_start, expr_end),
            name: "extracted".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(matches!(
        err,
        SemanticRefactorError::VarNotAllowedForInitializer
    ));
}

#[test]
fn extract_variable_rejects_var_for_method_reference_initializer() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  interface IntSupplier {
    int getAsInt();
  }

  static int foo() { return 1; }

  void m() {
    IntSupplier s = Test::foo;
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let expr_start = src.find("Test::foo").unwrap();
    let expr_end = expr_start + "Test::foo".len();

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(expr_start, expr_end),
            name: "extracted".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(matches!(
        err,
        SemanticRefactorError::VarNotAllowedForInitializer
    ));
}

#[test]
fn extract_variable_rejects_var_for_array_initializer() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int[] xs = {1,2};
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let expr_start = src.find("{1,2}").unwrap();
    let expr_end = expr_start + "{1,2}".len();

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(expr_start, expr_end),
            name: "extracted".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(matches!(
        err,
        SemanticRefactorError::VarNotAllowedForInitializer
    ));
}

#[test]
fn extract_variable_allows_explicit_type_for_array_initializer() {
    let file = FileId::new("Test.java");
    let fixture = r#"class Test {
  void m() {
    int[] xs = /*select*/{1,2}/*end*/;
  }
}
"#;

    let (src, expr_range) = strip_selection_markers(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.clone())]);

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "tmp".into(),
            use_var: false,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    let expected = r#"class Test {
  void m() {
    int[] tmp = {1,2};
    int[] xs = tmp;
  }
}
"#;

    assert_eq!(after, expected);
}

#[test]
fn extract_variable_use_var_false_emits_explicit_type() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int x = 1 + 2;
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let expr_start = src.find("1 + 2").unwrap();
    let expr_end = expr_start + "1 + 2".len();

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(expr_start, expr_end),
            name: "sum".into(),
            use_var: false,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    let expected = r#"class Test {
  void m() {
    int sum = 1 + 2;
    int x = sum;
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_prefers_typeck_type_when_use_var_false() {
    let file = FileId::new("Test.java");
    let fixture = r#"class C {
  void m() {
    String s = "hi";
    Object x = /*start*/s/*end*/;
  }
}
"#;
    let (src, expr_range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.clone())]);

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "y".into(),
            use_var: false,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    let expected = r#"class C {
  void m() {
    String s = "hi";
    String y = s;
    Object x = y;
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_prefers_typeck_when_it_adds_generics() {
    let file = FileId::new("Test.java");
    let fixture = r#"import java.util.List;

class Test {
  void m() {
    List<String> xs = null;
    Object y = /*select*/xs/*end*/;
  }
}
"#;

    let (src, expr_range) = strip_selection_markers(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.clone())]);

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "tmp".into(),
            use_var: false,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    let expected = r#"import java.util.List;

class Test {
  void m() {
    List<String> xs = null;
    List<String> tmp = xs;
    Object y = tmp;
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_use_var_false_errors_when_type_inference_unavailable() {
    let file = FileId::new("Test.java");
    let fixture = r#"class C {
  void m() {
    /*start*/foo()/*end*/;
  }
}
"#;

    let (src, expr_range) = extract_range(fixture);
    let db = TextDatabase::new([(file.clone(), src)]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file,
            expr_range,
            name: "tmp".into(),
            use_var: false,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(err, SemanticRefactorError::TypeInferenceFailed),
        "expected TypeInferenceFailed error, got: {err:?}"
    );
    assert_eq!(
        err.to_string(),
        "could not infer type for extracted expression"
    );
}

#[test]
fn extract_variable_explicit_types_are_inferred_for_common_expressions() {
    let file = FileId::new("Test.java");

    let cases = [
        (
            r#"class Test {
  void m() {
    boolean x = /*select*/true/*end*/;
  }
}
"#,
            r#"class Test {
  void m() {
    boolean value = true;
    boolean x = value;
  }
}
"#,
        ),
        (
            r#"class Test {
  void m() {
    char x = /*select*/'x'/*end*/;
  }
}
"#,
            r#"class Test {
  void m() {
    char value = 'x';
    char x = value;
  }
}
"#,
        ),
        (
            r#"class Test {
  void m() {
    String x = /*select*/"hi"/*end*/;
  }
}
"#,
            r#"class Test {
  void m() {
    String value = "hi";
    String x = value;
  }
}
"#,
        ),
        (
            r#"class Foo {}

class Test {
  void m() {
    Foo foo = null;
    Foo x = /*select*/foo/*end*/;
  }
}
"#,
            r#"class Foo {}

class Test {
  void m() {
    Foo foo = null;
    Foo value = foo;
    Foo x = value;
  }
}
"#,
        ),
        (
            r#"class Test {
  void m(Object x) {
    String y = /*select*/(String) x/*end*/;
  }
}
"#,
            r#"class Test {
  void m(Object x) {
    String value = (String) x;
    String y = value;
  }
}
"#,
        ),
        (
            r#"interface A {}
interface B {}

class Test {
  void m(boolean cond, A a, B b) {
    Object x = /*select*/cond ? a : b/*end*/;
  }
}
"#,
            r#"interface A {}
interface B {}

class Test {
  void m(boolean cond, A a, B b) {
    Object value = cond ? a : b;
    Object x = value;
  }
}
"#,
        ),
        (
            r#"class Test {
  void m() {
    String x = /*select*/null/*end*/;
  }
}
"#,
            r#"class Test {
  void m() {
    String value = null;
    String x = value;
  }
}
"#,
        ),
        (
            r#"class Test {
  void m() {
    boolean x = /*select*/1 < 2/*end*/;
  }
}
"#,
            r#"class Test {
  void m() {
    boolean value = 1 < 2;
    boolean x = value;
  }
}
"#,
        ),
        (
            r#"class Test {
  void m() {
    boolean x = /*select*/!true/*end*/;
  }
}
"#,
            r#"class Test {
  void m() {
    boolean value = !true;
    boolean x = value;
  }
}
"#,
        ),
    ];

    for (src, expected) in cases {
        let (src, range) = strip_selection_markers(src);
        let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

        let edit = extract_variable(
            &db,
            ExtractVariableParams {
                file: file.clone(),
                expr_range: range,
                name: "value".into(),
                use_var: false,
                replace_all: false,
            },
        )
        .unwrap();

        let after = apply_text_edits(&src, &edit.text_edits).unwrap();
        assert_eq!(after, expected);
    }
}

#[test]
fn extract_variable_plus_does_not_infer_string_from_nested_string_literal() {
    // Regression test: parser-only type inference for `+` should only infer `String` when either
    // operand is already inferred `String` (not when a nested string literal exists somewhere in
    // the expression subtree).
    let file = FileId::new("Test.java");
    let fixture = r#"class Test {
  void m() {
    System.out.println(/*start*/1 + ("x" == "y" ? 1 : 2)/*end*/);
  }
}
"#;

    let (src, expr_range) = extract_range(fixture);
    let db = TextDatabase::new([(file.clone(), src.clone())]);

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "tmp".into(),
            use_var: false,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    assert!(
        after.contains("int tmp = 1 + (\"x\" == \"y\" ? 1 : 2);"),
        "expected extraction of `1 + (\\\"x\\\" == \\\"y\\\" ? 1 : 2)`: {after}"
    );
    assert!(
        !after.contains("String tmp"),
        "expected `tmp` to not be inferred as String: {after}"
    );
}

#[test]
fn extract_variable_trims_whitespace_in_selection_range() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int x =  1 + 2  ;
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let expr_start = src.find("1 + 2").unwrap();
    let expr_end = expr_start + "1 + 2".len();

    // Select extra whitespace around the expression; the refactoring should
    // trim it and still find the expression node.
    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(expr_start - 2, expr_end + 2),
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    let expected = r#"class Test {
  void m() {
    var sum = 1 + 2;
    int x =  sum;
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_rejects_when_other_side_effects_exist_outside_selection() {
    let file = FileId::new("Test.java");
    let (src, expr_range) = strip_selection_markers(
        r#"class Test {
  int foo() { return 1; }
  int bar() { return 2; }
  void m() {
    int y = foo() + /*select*/bar()/*end*/;
  }
}
"#,
    );

    let db = RefactorJavaDatabase::new([(file.clone(), src)]);
    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file,
            expr_range,
            name: "tmp".into(),
            use_var: false,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(
            err,
            SemanticRefactorError::ExtractNotSupported { reason }
                if reason == EXTRACT_VARIABLE_EVAL_ORDER_GUARD_REASON
        ),
        "expected ExtractNotSupported error, got: {err:?}"
    );
}

#[test]
fn extract_variable_rejects_when_inc_dec_outside_selection() {
    let file = FileId::new("Test.java");
    let (src, expr_range) = strip_selection_markers(
        r#"class Test {
  int foo() { return 1; }
  void m() {
    int[] arr = new int[1];
    int i = 0;
    arr[i++] = /*select*/foo()/*end*/;
  }
}
"#,
    );

    let db = RefactorJavaDatabase::new([(file.clone(), src)]);
    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file,
            expr_range,
            name: "tmp".into(),
            use_var: false,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(
            err,
            SemanticRefactorError::ExtractNotSupported { reason }
                if reason == EXTRACT_VARIABLE_EVAL_ORDER_GUARD_REASON
        ),
        "expected ExtractNotSupported error, got: {err:?}"
    );
}

#[test]
fn extract_variable_allows_when_only_side_effect_is_inside_selection() {
    let file = FileId::new("Test.java");
    let (src, expr_range) = strip_selection_markers(
        r#"class Test {
  int foo() { return 1; }
  void m() {
    int y = /*select*/foo()/*end*/ + 1;
  }
}
"#,
    );

    let db = RefactorJavaDatabase::new([(file.clone(), src.clone())]);
    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "tmp".into(),
            use_var: false,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    assert!(
        after.contains("int tmp = foo();"),
        "expected extracted declaration: {after}"
    );
    assert!(
        after.contains("int y = tmp + 1;"),
        "expected replaced usage: {after}"
    );
}

#[test]
fn extract_variable_allows_pure_arithmetic_extraction_in_binary_expression() {
    let file = FileId::new("Test.java");
    let (src, expr_range) = strip_selection_markers(
        r#"class Test {
  void m() {
    int x = 0;
    int y = x + /*select*/(1 + 2)/*end*/;
  }
}
"#,
    );

    let db = RefactorJavaDatabase::new([(file.clone(), src.clone())]);
    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    assert!(
        after.contains("var sum = (1 + 2);"),
        "expected extracted declaration: {after}"
    );
    assert!(
        after.contains("int y = x + sum;"),
        "expected replaced usage: {after}"
    );
}

#[test]
fn extract_variable_splits_multi_declarator_when_initializer_depends_on_earlier_declarator() {
    let file = FileId::new("Test.java");
    let fixture = r#"class C {
  void m() {
    int b = 1, a = /*start*/b + 1/*end*/;
    System.out.println(a);
  }
}
"#;
    let (src, range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(range.start, range.end),
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    let expected = r#"class C {
  void m() {
    int b = 1;
    var sum = b + 1;
    int a = sum;
    System.out.println(a);
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_allows_initializer_in_first_declarator_of_multi_declarator_statement() {
    let file = FileId::new("Test.java");
    let fixture = r#"class C {
  void m() {
    int b = /*start*/1 + 2/*end*/, a = b + 1;
  }
}
"#;
    let (src, range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(range.start, range.end),
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    let expected = r#"class C {
  void m() {
    var sum = 1 + 2;
    int b = sum, a = b + 1;
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_splits_multi_declarator_when_initializer_depends_on_earlier_declarator_and_outer_names_exist(
) {
    let file = FileId::new("Test.java");
    let fixture = r#"class C {
  void m() {
    int b = 10;
    int b2 = 1, a = /*start*/b2 + b/*end*/;
  }
}
"#;
    let (src, range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(range.start, range.end),
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    let expected = r#"class C {
  void m() {
    int b = 10;
    int b2 = 1;
    var sum = b2 + b;
    int a = sum;
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_allows_initializer_that_depends_on_earlier_declarator_with_qualified_name() {
    let file = FileId::new("Test.java");
    let fixture = r#"class C {
  static class Node { Node next; }
  void m() {
    Node b = new Node(), a = /*start*/b.next/*end*/;
  }
}
"#;
    let (src, range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(range.start, range.end),
            name: "n".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    let expected = r#"class C {
  static class Node { Node next; }
  void m() {
    Node b = new Node();
    var n = b.next;
    Node a = n;
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_rejects_expression_bodied_lambda() {
    let file = FileId::new("Test.java");
    let fixture = r#"import java.util.function.Function;
 class C {
   void m() {
     Function<Integer,Integer> f = x -> /*start*/x + 1/*end*/;
   }
 }
 "#;
    let (src, expr_range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file,
            expr_range: WorkspaceTextRange::new(expr_range.start, expr_range.end),
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(
            err,
            SemanticRefactorError::ExtractNotSupported { reason }
                if reason == "cannot extract from expression-bodied lambda body"
        ),
        "expected expression-bodied lambda rejection, got: {err:?}"
    );
}

#[test]
fn extract_variable_rejects_expression_bodied_lambda_nested_expression() {
    let file = FileId::new("Test.java");
    let fixture = r#"class Test {
  void m() {
    Runnable r = () -> System.out.println(/*start*/1 + 2/*end*/);
  }
}
"#;
    let (src, expr_range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file,
            expr_range: WorkspaceTextRange::new(expr_range.start, expr_range.end),
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(
            err,
            SemanticRefactorError::ExtractNotSupported { reason }
                if reason == "cannot extract from expression-bodied lambda body"
        ),
        "expected expression-bodied lambda rejection, got: {err:?}"
    );
}

#[test]
fn extract_variable_allows_block_bodied_lambda() {
    let file = FileId::new("Test.java");
    let fixture = r#"class C {
  void m() {
    Runnable r = () -> {
      System.out.println(/*start*/1 + 2/*end*/);
    };
  }
}
"#;
    let (src, expr_range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(expr_range.start, expr_range.end),
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    let expected = r#"class C {
  void m() {
    Runnable r = () -> {
      var sum = 1 + 2;
      System.out.println(sum);
    };
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_rejects_name_conflict_with_lambda_parameter() {
    let file = FileId::new("Test.java");
    let fixture = r#"class C {
  void m() {
    java.util.function.IntConsumer c = (sum) -> {
      System.out.println(/*start*/1 + 2/*end*/);
    };
  }
}
"#;
    let (src, expr_range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file,
            expr_range: WorkspaceTextRange::new(expr_range.start, expr_range.end),
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    let SemanticRefactorError::Conflicts(conflicts) = err else {
        panic!("expected conflicts, got: {err:?}");
    };

    assert!(
        conflicts
            .iter()
            .any(|c| matches!(c, Conflict::NameCollision { name, .. } if name == "sum")),
        "expected NameCollision conflict: {conflicts:?}"
    );
}

#[test]
fn extract_variable_rejects_name_conflict_with_lambda_parameter_in_later_statement() {
    let file = FileId::new("Test.java");
    let fixture = r#"class C {
  void m() {
    System.out.println(/*start*/1 + 2/*end*/);
    java.util.function.IntConsumer c = (sum) -> {
      System.out.println(sum);
    };
  }
}
"#;
    let (src, expr_range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file,
            expr_range: WorkspaceTextRange::new(expr_range.start, expr_range.end),
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    let SemanticRefactorError::Conflicts(conflicts) = err else {
        panic!("expected conflicts, got: {err:?}");
    };

    assert!(
        conflicts
            .iter()
            .any(|c| matches!(c, Conflict::NameCollision { name, .. } if name == "sum")),
        "expected NameCollision conflict: {conflicts:?}"
    );
}

#[test]
fn extract_variable_allows_lambda_parameter_name_after_lambda_scope() {
    let file = FileId::new("Test.java");
    let fixture = r#"class C {
  void m() {
    java.util.function.IntConsumer c = (sum) -> {
      System.out.println(sum);
    };
    System.out.println(/*start*/1 + 2/*end*/);
  }
}
"#;
    let (src, expr_range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(expr_range.start, expr_range.end),
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    let expected = r#"class C {
  void m() {
    java.util.function.IntConsumer c = (sum) -> {
      System.out.println(sum);
    };
    var sum = 1 + 2;
    System.out.println(sum);
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_allows_lambda_parameter_when_name_does_not_conflict() {
    let file = FileId::new("Test.java");
    let fixture = r#"class C {
  void m() {
    java.util.function.IntConsumer c = (sum) -> {
      System.out.println(/*start*/1 + 2/*end*/);
    };
  }
}
"#;
    let (src, expr_range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(expr_range.start, expr_range.end),
            name: "tmp".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    let expected = r#"class C {
  void m() {
    java.util.function.IntConsumer c = (sum) -> {
      var tmp = 1 + 2;
      System.out.println(tmp);
    };
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_rejects_name_conflict_with_catch_parameter() {
    let file = FileId::new("Test.java");
    let fixture = r#"class C {
  void m() {
    try {
    } catch (Exception e) {
      System.out.println(/*start*/1 + 2/*end*/);
    }
  }
}
"#;
    let (src, expr_range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file,
            expr_range: WorkspaceTextRange::new(expr_range.start, expr_range.end),
            name: "e".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    let SemanticRefactorError::Conflicts(conflicts) = err else {
        panic!("expected conflicts, got: {err:?}");
    };

    assert!(
        conflicts
            .iter()
            .any(|c| matches!(c, Conflict::NameCollision { name, .. } if name == "e")),
        "expected NameCollision conflict: {conflicts:?}"
    );
}

#[test]
fn extract_variable_rejects_name_conflict_with_catch_parameter_in_later_statement() {
    let file = FileId::new("Test.java");
    let fixture = r#"class C {
  void m() {
    System.out.println(/*start*/1 + 2/*end*/);
    try {
    } catch (Exception e) {
      System.out.println(e);
    }
  }
}
"#;
    let (src, expr_range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file,
            expr_range: WorkspaceTextRange::new(expr_range.start, expr_range.end),
            name: "e".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    let SemanticRefactorError::Conflicts(conflicts) = err else {
        panic!("expected conflicts, got: {err:?}");
    };

    assert!(
        conflicts
            .iter()
            .any(|c| matches!(c, Conflict::NameCollision { name, .. } if name == "e")),
        "expected NameCollision conflict: {conflicts:?}"
    );
}

#[test]
fn extract_variable_allows_catch_parameter_name_after_catch_scope() {
    let file = FileId::new("Test.java");
    let fixture = r#"class C {
  void m() {
    try {
    } catch (Exception e) {
      System.out.println(e);
    }
    System.out.println(/*start*/1 + 2/*end*/);
  }
}
"#;
    let (src, expr_range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(expr_range.start, expr_range.end),
            name: "e".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    let expected = r#"class C {
  void m() {
    try {
    } catch (Exception e) {
      System.out.println(e);
    }
    var e = 1 + 2;
    System.out.println(e);
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_rejects_name_conflict_with_pattern_variable() {
    let file = FileId::new("Test.java");
    let fixture = r#"class C {
  void m(Object obj) {
    if (obj instanceof String s) {
      System.out.println(/*start*/1+2/*end*/);
    }
  }
}
"#;
    let (src, expr_range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file,
            expr_range: WorkspaceTextRange::new(expr_range.start, expr_range.end),
            name: "s".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    let SemanticRefactorError::Conflicts(conflicts) = err else {
        panic!("expected conflicts, got: {err:?}");
    };

    assert!(
        conflicts
            .iter()
            .any(|c| matches!(c, Conflict::NameCollision { name, .. } if name == "s")),
        "expected NameCollision conflict: {conflicts:?}"
    );
}

#[test]
fn extract_variable_rejects_name_conflict_with_pattern_variable_flow_scope() {
    let file = FileId::new("Test.java");
    let fixture = r#"class C {
  void m(Object obj) {
    if (!(obj instanceof String s)) return;
    System.out.println(/*start*/1+2/*end*/);
  }
}
"#;
    let (src, expr_range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file,
            expr_range: WorkspaceTextRange::new(expr_range.start, expr_range.end),
            name: "s".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    let SemanticRefactorError::Conflicts(conflicts) = err else {
        panic!("expected conflicts, got: {err:?}");
    };

    assert!(
        conflicts
            .iter()
            .any(|c| matches!(c, Conflict::NameCollision { name, .. } if name == "s")),
        "expected NameCollision conflict: {conflicts:?}"
    );
}

#[test]
fn extract_variable_rejects_name_conflict_with_pattern_variable_flow_scope_multistmt_guard() {
    let file = FileId::new("Test.java");
    let fixture = r#"class C {
  void m(Object obj) {
    if (!(obj instanceof String s)) {
      System.out.println(obj);
      return;
    }
    System.out.println(/*start*/1+2/*end*/);
  }
}
"#;
    let (src, expr_range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file,
            expr_range: WorkspaceTextRange::new(expr_range.start, expr_range.end),
            name: "s".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    let SemanticRefactorError::Conflicts(conflicts) = err else {
        panic!("expected conflicts, got: {err:?}");
    };

    assert!(
        conflicts
            .iter()
            .any(|c| matches!(c, Conflict::NameCollision { name, .. } if name == "s")),
        "expected NameCollision conflict: {conflicts:?}"
    );
}

#[test]
fn extract_variable_rejects_name_conflict_with_pattern_variable_flow_scope_else_guard() {
    let file = FileId::new("Test.java");
    let fixture = r#"class C {
  void m(Object obj) {
    if (obj instanceof String s) {
    } else {
      System.out.println(obj);
      return;
    }
    System.out.println(/*start*/1+2/*end*/);
  }
}
"#;
    let (src, expr_range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file,
            expr_range: WorkspaceTextRange::new(expr_range.start, expr_range.end),
            name: "s".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    let SemanticRefactorError::Conflicts(conflicts) = err else {
        panic!("expected conflicts, got: {err:?}");
    };

    assert!(
        conflicts
            .iter()
            .any(|c| matches!(c, Conflict::NameCollision { name, .. } if name == "s")),
        "expected NameCollision conflict: {conflicts:?}"
    );
}

#[test]
fn extract_variable_allows_name_reuse_in_else_branch_when_pattern_in_then_branch() {
    let file = FileId::new("Test.java");
    let fixture = r#"class C {
  void m(Object obj) {
    if (obj instanceof String s) {
      System.out.println(s);
    } else {
      System.out.println(/*start*/1 + 2/*end*/);
    }
  }
}
"#;
    let (src, expr_range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(expr_range.start, expr_range.end),
            name: "s".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    let expected = r#"class C {
  void m(Object obj) {
    if (obj instanceof String s) {
      System.out.println(s);
    } else {
      var s = 1 + 2;
      System.out.println(s);
    }
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_allows_name_reuse_in_do_while_body_when_pattern_in_condition() {
    let file = FileId::new("Test.java");
    let fixture = r#"class C {
  void m(Object obj) {
    do {
      System.out.println(/*start*/1 + 2/*end*/);
    } while (obj instanceof String s);
  }
}
"#;
    let (src, expr_range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(expr_range.start, expr_range.end),
            name: "s".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    let expected = r#"class C {
  void m(Object obj) {
    do {
      var s = 1 + 2;
      System.out.println(s);
    } while (obj instanceof String s);
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_rejects_name_conflict_with_for_loop_variable() {
    let file = FileId::new("Test.java");
    let fixture = r#"class C {
  void m() {
    for (int i = 0; i < 1; i++) {
      int x = /*start*/1 + 2/*end*/;
    }
  }
}
"#;
    let (src, expr_range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file,
            expr_range: WorkspaceTextRange::new(expr_range.start, expr_range.end),
            name: "i".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    let SemanticRefactorError::Conflicts(conflicts) = err else {
        panic!("expected conflicts, got: {err:?}");
    };

    assert!(
        conflicts
            .iter()
            .any(|c| matches!(c, Conflict::NameCollision { name, .. } if name == "i")),
        "expected NameCollision conflict: {conflicts:?}"
    );
}

#[test]
fn extract_variable_allows_reusing_name_after_for_loop_scope_ends() {
    let file = FileId::new("Test.java");
    let fixture = r#"class C {
  void m() {
    for (int i = 0; i < 1; i++) {
    }
    int x = /*start*/1 + 2/*end*/;
  }
}
"#;
    let (src, expr_range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(expr_range.start, expr_range.end),
            name: "i".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    let expected = r#"class C {
  void m() {
    for (int i = 0; i < 1; i++) {
    }
    var i = 1 + 2;
    int x = i;
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_rejects_name_conflict_with_enhanced_for_variable() {
    let file = FileId::new("Test.java");
    let fixture = r#"class C {
  void m(java.util.List<Integer> xs) {
    for (int i : xs) {
      int x = /*start*/1 + 2/*end*/;
    }
  }
}
"#;
    let (src, expr_range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file,
            expr_range: WorkspaceTextRange::new(expr_range.start, expr_range.end),
            name: "i".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    let SemanticRefactorError::Conflicts(conflicts) = err else {
        panic!("expected conflicts, got: {err:?}");
    };

    assert!(
        conflicts
            .iter()
            .any(|c| matches!(c, Conflict::NameCollision { name, .. } if name == "i")),
        "expected NameCollision conflict: {conflicts:?}"
    );
}

#[test]
fn extract_variable_rejects_switch_expression_rule_expression() {
    let file = FileId::new("Test.java");
    let fixture = r#"class Test {
  int m(int x) {
    int y = switch (x) {
      case 1 -> /*start*/1 + 2/*end*/;
      default -> 0;
    };
    return y;
  }
}
"#;
    let (src, expr_range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file,
            expr_range: WorkspaceTextRange::new(expr_range.start, expr_range.end),
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(
            err,
            SemanticRefactorError::ExtractNotSupported { reason }
                if reason == "cannot extract from non-block switch rule body"
        ),
        "expected switch expression rule rejection, got: {err:?}"
    );
}

#[test]
fn extract_variable_rejects_switch_expression_rule_expression_nested() {
    let file = FileId::new("Test.java");
    let fixture = r#"class Test {
  static int foo(int x) { return x; }

  int m(int x) {
    int y = switch (x) {
      case 1 -> foo(/*start*/1 + 2/*end*/);
      default -> 0;
    };
    return y;
  }
}
"#;
    let (src, expr_range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file,
            expr_range: WorkspaceTextRange::new(expr_range.start, expr_range.end),
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(
            err,
            SemanticRefactorError::ExtractNotSupported { reason }
                if reason == "cannot extract from non-block switch rule body"
        ),
        "expected non-block switch rule body rejection, got: {err:?}"
    );
}

#[test]
fn extract_variable_rejects_switch_case_label_expression() {
    let file = FileId::new("Test.java");
    let fixture =
        r#"class C { void m(int x) { switch (x) { case /*start*/1 + 2/*end*/: break; } } }"#;
    let (src, expr_range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file,
            expr_range: WorkspaceTextRange::new(expr_range.start, expr_range.end),
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(
            err,
            SemanticRefactorError::ExtractNotSupported { reason }
                if reason == "cannot extract from switch labels"
        ),
        "expected switch label rejection, got: {err:?}"
    );
}

#[test]
fn extract_variable_rejects_switch_expression_rule_throw_statement_body() {
    let file = FileId::new("Test.java");
    let fixture = r#"class Test {
  int m(int x, RuntimeException ex) {
    int y = switch (x) {
      case 1 ->
        throw /*start*/ex/*end*/;
      default -> 0;
    };
    return y;
  }
}
"#;
    let (src, expr_range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file,
            expr_range: WorkspaceTextRange::new(expr_range.start, expr_range.end),
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(
            err,
            SemanticRefactorError::ExtractNotSupported { reason }
                if reason == "cannot extract from non-block switch rule body"
        ),
        "expected non-block switch rule body rejection, got: {err:?}"
    );
}

#[test]
fn extract_variable_rejects_arrow_switch_rule_statement_body() {
    let file = FileId::new("Test.java");
    let fixture = r#"class C { void m(int x) { switch (x) { case 1 -> System.out.println(/*start*/1 + 2/*end*/); default -> {} } } }"#;
    let (src, expr_range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file,
            expr_range: WorkspaceTextRange::new(expr_range.start, expr_range.end),
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(
            err,
            SemanticRefactorError::ExtractNotSupported { reason }
                if reason == "cannot extract into a single-statement switch rule body without braces"
        ),
        "expected arrow switch rule statement-body rejection, got: {err:?}"
    );
}

#[test]
fn extract_variable_rejects_arrow_switch_rule_statement_body_multiline() {
    let file = FileId::new("Test.java");
    let fixture = r#"class Test {
  void m(int x) {
    switch (x) {
      case 1 ->
        System.out.println(/*start*/1 + 2/*end*/);
      default -> {}
    }
  }
}
"#;
    let (src, expr_range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file,
            expr_range: WorkspaceTextRange::new(expr_range.start, expr_range.end),
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(
            err,
            SemanticRefactorError::ExtractNotSupported { reason }
                if reason == "cannot extract into a single-statement switch rule body without braces"
        ),
        "expected arrow switch rule statement-body rejection, got: {err:?}"
    );
}

#[test]
fn extract_variable_allows_arrow_switch_rule_block_body() {
    let file = FileId::new("Test.java");
    let fixture = r#"class C { void m(int x) { switch (x) { case 1 -> { System.out.println(/*start*/1 + 2/*end*/); } default -> {} } } }"#;
    let (src, expr_range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(expr_range.start, expr_range.end),
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    let expected = r#"class C { void m(int x) { switch (x) { case 1 -> { var sum = 1 + 2; System.out.println(sum); } default -> {} } } }"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_rejects_while_condition_extraction() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int i = 0;
    while (i < 10) {
      i++;
    }

    do {
      i++;
    } while (i < 10);
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    // while condition
    let expr_start = src.find("i < 10").unwrap();
    let expr_end = expr_start + "i < 10".len();
    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(expr_start, expr_end),
            name: "cond".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();
    assert!(
        matches!(err, SemanticRefactorError::ExtractNotSupported { .. }),
        "expected ExtractNotSupported error, got: {err:?}"
    );

    // do-while condition (second occurrence)
    let expr_start = src.rfind("i < 10").unwrap();
    let expr_end = expr_start + "i < 10".len();
    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file,
            expr_range: WorkspaceTextRange::new(expr_start, expr_end),
            name: "cond".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();
    assert!(
        matches!(err, SemanticRefactorError::ExtractNotSupported { .. }),
        "expected ExtractNotSupported error, got: {err:?}"
    );
}

#[test]
fn extract_variable_rejects_for_condition_or_update_extraction() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m(int n, int step) {
    for (int i = 0; i < n; i += step) {
    }
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    // Condition selection (`n` in `i < n`).
    let cond_start = src.find("i < n").unwrap() + "i < ".len();
    let cond_end = cond_start + "n".len();
    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(cond_start, cond_end),
            name: "limit".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();
    assert!(
        matches!(err, SemanticRefactorError::ExtractNotSupported { .. }),
        "expected ExtractNotSupported error, got: {err:?}"
    );

    // Update selection (`step` in `i += step`).
    let update_start = src.find("i += step").unwrap() + "i += ".len();
    let update_end = update_start + "step".len();
    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file,
            expr_range: WorkspaceTextRange::new(update_start, update_end),
            name: "s".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();
    assert!(
        matches!(err, SemanticRefactorError::ExtractNotSupported { .. }),
        "expected ExtractNotSupported error, got: {err:?}"
    );
}

#[test]
fn extract_variable_rejects_for_condition() {
    let file = FileId::new("Test.java");
    let fixture = r#"class C {
  void m() {
    for (int i = 0; /*start*/i < 10/*end*/; i++) {
      System.out.println(i);
    }
  }
}
"#;

    let (src, expr_range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src)]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file,
            expr_range,
            name: "cond".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(err, SemanticRefactorError::ExtractNotSupported { .. }),
        "expected ExtractNotSupported error, got: {err:?}"
    );
}

#[test]
fn extract_variable_still_works_inside_for_body() {
    let file = FileId::new("Test.java");
    let fixture = r#"class C {
  void m() {
    for (int i = 0; i < 10; i++) {
      System.out.println(/*start*/1 + 2/*end*/);
    }
  }
}
"#;

    let (src, expr_range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.clone())]);

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    let expected = r#"class C {
  void m() {
    for (int i = 0; i < 10; i++) {
      var sum = 1 + 2;
      System.out.println(sum);
    }
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_rejects_short_circuit_rhs_extraction() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  static class Box { int value; }

  void m(Box b) {
    if (b != null && b.value > 0) {
    }
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    // RHS field access: `b.value` in `b.value > 0`.
    let expr_start = src.find("b.value > 0").unwrap();
    let expr_end = expr_start + "b.value".len();
    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file,
            expr_range: WorkspaceTextRange::new(expr_start, expr_end),
            name: "v".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(err, SemanticRefactorError::ExtractNotSupported { .. }),
        "expected ExtractNotSupported error, got: {err:?}"
    );
}

#[test]
fn extract_variable_rejects_ternary_branch_extraction() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  static class Box { int value; }

  int m(boolean cond, Box b) {
    return cond ? b.value : 0;
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    // Then-branch field access: `b.value`.
    let expr_start = src.find("b.value :").unwrap();
    let expr_end = expr_start + "b.value".len();
    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file,
            expr_range: WorkspaceTextRange::new(expr_start, expr_end),
            name: "v".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(err, SemanticRefactorError::ExtractNotSupported { .. }),
        "expected ExtractNotSupported error, got: {err:?}"
    );
}

#[test]
fn extract_variable_rejects_method_call_expression() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int x = foo();
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let expr_start = src.find("foo()").unwrap();
    let expr_end = expr_start + "foo()".len();

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(expr_start, expr_end),
            name: "call".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(err, SemanticRefactorError::ExtractSideEffects),
        "expected ExtractSideEffects error, got: {err:?}"
    );
    assert_eq!(
        err.to_string(),
        "expression has side effects and cannot be extracted safely"
    );
}

#[test]
fn extract_variable_rejects_new_expression() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    Object x = new Object();
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let expr_start = src.find("new Object()").unwrap();
    let expr_end = expr_start + "new Object()".len();

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(expr_start, expr_end),
            name: "obj".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(err, SemanticRefactorError::ExtractSideEffects),
        "expected ExtractSideEffects error, got: {err:?}"
    );
    assert_eq!(
        err.to_string(),
        "expression has side effects and cannot be extracted safely"
    );
}

#[test]
fn extract_variable_rejects_super_constructor_invocation_argument() {
    let file = FileId::new("Test.java");
    let fixture = r#"class B { B(int x) {} }
class C extends B {
  C() { super(/*start*/1 + 2/*end*/); }
}
"#;

    let (src, expr_range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src)]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(
            err,
            SemanticRefactorError::ExtractNotSupported { reason }
                if reason
                    == "cannot extract from explicit constructor invocation (`this(...)` / `super(...)`)"
        ),
        "expected ExtractNotSupported error, got: {err:?}"
    );
}

#[test]
fn extract_variable_rejects_this_constructor_invocation_argument() {
    let file = FileId::new("Test.java");
    let fixture = r#"class C {
  C(int x) {}
  C() { this(/*start*/1 + 2/*end*/); }
}
"#;

    let (src, expr_range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src)]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(
            err,
            SemanticRefactorError::ExtractNotSupported { reason }
                if reason
                    == "cannot extract from explicit constructor invocation (`this(...)` / `super(...)`)"
        ),
        "expected ExtractNotSupported error, got: {err:?}"
    );
}

#[test]
fn extract_variable_allows_extraction_after_super_constructor_invocation() {
    let file = FileId::new("Test.java");
    let fixture = r#"class B { B(int x) {} }
class C extends B {
  C() {
    super(0);
    int x = /*start*/1 + 2/*end*/;
  }
}
"#;

    let (src, expr_range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    let expected = r#"class B { B(int x) {} }
class C extends B {
  C() {
    super(0);
    var sum = 1 + 2;
    int x = sum;
  }
}
"#;

    assert_eq!(after, expected);
}

#[test]
fn extract_variable_allows_extraction_after_this_constructor_invocation() {
    let file = FileId::new("Test.java");
    let fixture = r#"class C {
  C(int x) {}
  C() {
    this(0);
    int y = /*start*/1 + 2/*end*/;
  }
}
"#;

    let (src, expr_range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    let expected = r#"class C {
  C(int x) {}
  C() {
    this(0);
    var sum = 1 + 2;
    int y = sum;
  }
}
"#;

    assert_eq!(after, expected);
}

#[test]
fn extract_variable_rejects_if_body_without_braces_multiline() {
    let file = FileId::new("Test.java");
    let fixture = r#"class Test {
  void m(boolean cond) {
    if (cond)
      System.out.println(/*start*/1+2/*end*/);
  }
}
"#;

    let (src, expr_range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src)]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(
            err,
            SemanticRefactorError::ExtractNotSupported { reason }
                if reason
                    == "cannot extract into a single-statement control structure body without braces"
        ),
        "expected ExtractNotSupported error, got: {err:?}"
    );
}

#[test]
fn extract_variable_rejects_if_body_without_braces_oneline() {
    let file = FileId::new("Test.java");
    let fixture = r#"class Test {
  void m(boolean cond) {
    if (cond) System.out.println(/*start*/1+2/*end*/);
  }
}
"#;

    let (src, expr_range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src)]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(
            err,
            SemanticRefactorError::ExtractNotSupported { reason }
                if reason
                    == "cannot extract into a single-statement control structure body without braces"
        ),
        "expected ExtractNotSupported error, got: {err:?}"
    );
}

#[test]
fn extract_variable_rejects_while_body_without_braces_multiline() {
    let file = FileId::new("Test.java");
    let fixture = r#"class Test {
  void m(boolean cond) {
    while (cond)
      System.out.println(/*start*/1 + 2/*end*/);
  }
}
"#;

    let (src, expr_range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src)]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(
            err,
            SemanticRefactorError::ExtractNotSupported { reason }
                if reason == "cannot extract into a single-statement control structure body without braces"
        ),
        "expected ExtractNotSupported error, got: {err:?}"
    );
}

#[test]
fn extract_variable_rejects_do_while_body_without_braces_multiline() {
    let file = FileId::new("Test.java");
    let fixture = r#"class Test {
  void m(boolean cond) {
    do
      System.out.println(/*start*/1 + 2/*end*/);
    while (cond);
  }
}
"#;

    let (src, expr_range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src)]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(
            err,
            SemanticRefactorError::ExtractNotSupported { reason }
                if reason == "cannot extract into a single-statement control structure body without braces"
        ),
        "expected ExtractNotSupported error, got: {err:?}"
    );
}

#[test]
fn extract_variable_rejects_for_body_without_braces_multiline() {
    let file = FileId::new("Test.java");
    let fixture = r#"class Test {
  void m() {
    for (int i = 0; i < 10; i++)
      System.out.println(/*start*/1 + 2/*end*/);
  }
}
"#;

    let (src, expr_range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src)]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(
            err,
            SemanticRefactorError::ExtractNotSupported { reason }
                if reason == "cannot extract into a single-statement control structure body without braces"
        ),
        "expected ExtractNotSupported error, got: {err:?}"
    );
}

#[test]
fn extract_variable_rejects_oneline_switch_case_statement() {
    let file = FileId::new("Test.java");
    let fixture = r#"class Test {
  void m(int x) {
    switch (x) { case 1: System.out.println(/*start*/1+2/*end*/); }
  }
}
"#;

    let (src, expr_range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src)]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(
            err,
            SemanticRefactorError::ExtractNotSupported { reason }
                if reason == "cannot extract when the enclosing statement starts mid-line"
        ),
        "expected ExtractNotSupported error, got: {err:?}"
    );
}

#[test]
fn extract_variable_allows_switch_case_group() {
    let file = FileId::new("Test.java");
    let fixture = r#"class Test {
  void m(int x) {
    switch (x) {
      case 1:
        System.out.println(/*select*/1 + 2/*end*/);
        break;
    }
  }
}
"#;

    let (src, expr_range) = strip_selection_markers(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.clone())]);

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    let expected = r#"class Test {
  void m(int x) {
    switch (x) {
      case 1:
        var sum = 1 + 2;
        System.out.println(sum);
        break;
    }
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_rejects_name_conflict_in_other_switch_case_group() {
    let file = FileId::new("Test.java");
    let fixture = r#"class Test {
  void m(int x) {
    switch (x) {
      case 1:
        int tmp = 0;
        break;
      case 2:
        System.out.println(/*start*/1 + 2/*end*/);
    }
  }
}
"#;

    let (src, expr_range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src)]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "tmp".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    let SemanticRefactorError::Conflicts(conflicts) = err else {
        panic!("expected conflicts, got: {err:?}");
    };

    assert!(
        conflicts
            .iter()
            .any(|c| matches!(c, Conflict::NameCollision { name, .. } if name == "tmp")),
        "expected NameCollision conflict, got: {conflicts:?}"
    );
}

#[test]
fn extract_variable_rejects_switch_arrow_single_statement_body() {
    let file = FileId::new("Test.java");
    let fixture = r#"class Test {
  void m(int x) {
    switch (x) {
      case 1 -> System.out.println(/*select*/1 + 2/*end*/);
    }
  }
}
"#;

    let (src, expr_range) = strip_selection_markers(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src)]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(
            err,
            SemanticRefactorError::ExtractNotSupported { reason }
                if reason == "cannot extract into a single-statement switch rule body without braces"
        ),
        "expected switch arrow rule rejection, got: {err:?}"
    );
}

#[test]
fn extract_variable_rejects_switch_arrow_multiline_single_statement_body() {
    let file = FileId::new("Test.java");
    let fixture = r#"class Test {
  void m(int x) {
    switch (x) {
      case 1 ->
        System.out.println(/*select*/1 + 2/*end*/);
      default -> {
        System.out.println(0);
      }
    }
  }
}
"#;

    let (src, expr_range) = strip_selection_markers(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src)]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(
            err,
            SemanticRefactorError::ExtractNotSupported { reason }
                if reason == "cannot extract into a single-statement switch rule body without braces"
        ),
        "expected switch arrow rule rejection, got: {err:?}"
    );
}

#[test]
fn extract_variable_rejects_labeled_statement_body() {
    let file = FileId::new("Test.java");
    let fixture = r#"class Test {
  void m() {
    label:
      System.out.println(/*select*/1 + 2/*end*/);
  }
}
"#;

    let (src, expr_range) = strip_selection_markers(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src)]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(
            err,
            SemanticRefactorError::ExtractNotSupported { reason }
                if reason == "cannot extract into a labeled statement body"
        ),
        "expected labeled statement rejection, got: {err:?}"
    );
}

#[test]
fn extract_variable_rejects_synchronized_body_without_braces_multiline() {
    let file = FileId::new("Test.java");
    let fixture = r#"class C {
  void m(Object lock) {
    synchronized(lock)
      System.out.println(/*start*/1 + 2/*end*/);
  }
}
"#;

    let (src, expr_range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src)]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    // Today this is rejected at parse time because Nova's Java parser requires a block
    // after `synchronized (...)`. If we later accept single-statement synchronized bodies,
    // we still want extraction to be rejected (it would need braces to preserve semantics).
    assert!(
        matches!(
            err,
            SemanticRefactorError::ParseError | SemanticRefactorError::ExtractNotSupported { .. }
        ),
        "expected ParseError or ExtractNotSupported, got: {err:?}"
    );
}

#[test]
fn extract_variable_allows_braced_synchronized_body_oneline() {
    let file = FileId::new("Test.java");
    let fixture = r#"class C {
  void m(Object lock) {
    synchronized(lock) { System.out.println(/*start*/1 + 2/*end*/); }
  }
}
"#;

    let (src, expr_range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.clone())]);

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    assert!(
        after.contains("synchronized(lock) { var sum = 1 + 2; System.out.println(sum); }"),
        "unexpected output: {after}"
    );
}

#[test]
fn extract_variable_allows_braced_labeled_statement_body_oneline() {
    let file = FileId::new("Test.java");
    let fixture = r#"class C {
  void m() {
    label: { System.out.println(/*start*/1 + 2/*end*/); }
  }
}
"#;

    let (src, expr_range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.clone())]);

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    assert!(
        after.contains("label: { var sum = 1 + 2; System.out.println(sum); }"),
        "unexpected output: {after}"
    );
}

#[test]
fn extract_variable_allows_extraction_inside_braced_if_block() {
    let file = FileId::new("Test.java");
    let fixture = r#"class Test {
  void m(boolean cond) {
    if (cond) {
      System.out.println(/*start*/1+2/*end*/);
    }
  }
}
"#;

    let (src, expr_range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.clone())]);

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    let expected = r#"class Test {
  void m(boolean cond) {
    if (cond) {
      var sum = 1+2;
      System.out.println(sum);
    }
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_rejects_try_with_resources_resource_specification() {
    let file = FileId::new("Test.java");
    let fixture = r#"class C {
  void m(java.io.InputStream in) throws Exception {
    try (java.io.BufferedInputStream r = new java.io.BufferedInputStream(/*start*/in/*end*/)) {
      r.read();
    }
  }
}
"#;

    let (src, expr_range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src)]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "x".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(
            err,
            SemanticRefactorError::ExtractNotSupported { reason }
                if reason == "cannot extract from try-with-resources resource specification"
        ),
        "expected ExtractNotSupported error, got: {err:?}"
    );
}

#[test]
fn extract_variable_allows_extraction_inside_try_body() {
    let file = FileId::new("Test.java");
    let fixture = r#"class C {
  void m(java.io.InputStream in) throws Exception {
    try (java.io.BufferedInputStream r = new java.io.BufferedInputStream(in)) {
      System.out.println(/*start*/1 + 2/*end*/);
    }
  }
}
"#;

    let (src, expr_range) = extract_range(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.clone())]);

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    let expected = r#"class C {
  void m(java.io.InputStream in) throws Exception {
    try (java.io.BufferedInputStream r = new java.io.BufferedInputStream(in)) {
      var sum = 1 + 2;
      System.out.println(sum);
    }
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_allows_extraction_from_if_condition_with_side_effects_in_body() {
    let file = FileId::new("Test.java");
    let fixture = r#"class Test {
  void m(Object a) {
    if (/*select*/a != null/*end*/) {
      foo();
    }
  }

  void foo() {}
}
"#;

    let (src, expr_range) = strip_selection_markers(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.clone())]);

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "nonNull".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    let expected = r#"class Test {
  void m(Object a) {
    var nonNull = a != null;
    if (nonNull) {
      foo();
    }
  }

  void foo() {}
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_allows_extraction_from_switch_selector_with_side_effects_in_body() {
    let file = FileId::new("Test.java");
    let fixture = r#"class Test {
  void m(int x) {
    switch (/*select*/x + 1/*end*/) {
      case 1:
        foo();
        break;
    }
  }

  void foo() {}
}
"#;

    let (src, expr_range) = strip_selection_markers(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.clone())]);

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "selector".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    let expected = r#"class Test {
  void m(int x) {
    var selector = x + 1;
    switch (selector) {
      case 1:
        foo();
        break;
    }
  }

  void foo() {}
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_allows_extraction_from_synchronized_lock_with_side_effects_in_body() {
    let file = FileId::new("Test.java");
    let fixture = r#"class Test {
  void m() {
    Object lockObj = new Object();
    synchronized (/*select*/lockObj/*end*/) {
      foo();
    }
  }

  void foo() {}
}
"#;

    let (src, expr_range) = strip_selection_markers(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.clone())]);

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "lockObj2".into(),
            use_var: false,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    let expected = r#"class Test {
  void m() {
    Object lockObj = new Object();
    Object lockObj2 = lockObj;
    synchronized (lockObj2) {
      foo();
    }
  }

  void foo() {}
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_allows_extraction_from_switch_expression_selector_with_side_effects_in_body() {
    let file = FileId::new("Test.java");
    let fixture = r#"class Test {
  int m(int x) {
    int y = switch (/*select*/x + 1/*end*/) {
      case 1 -> { foo(); yield 1; }
      default -> 0;
    };
    return y;
  }

  void foo() {}
}
"#;

    let (src, expr_range) = strip_selection_markers(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.clone())]);

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "selector".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    let expected = r#"class Test {
  int m(int x) {
    var selector = x + 1;
    int y = switch (selector) {
      case 1 -> { foo(); yield 1; }
      default -> 0;
    };
    return y;
  }

  void foo() {}
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_rejects_switch_expression_selector_extraction_when_it_would_reorder_other_side_effects(
) {
    let file = FileId::new("Test.java");
    let fixture = r#"class Test {
  int foo() { return 0; }
  void bar() {}

  int m(int x) {
    int y = foo() + switch (/*select*/x + 1/*end*/) {
      case 1 -> { bar(); yield 1; }
      default -> 0;
    };
    return y;
  }
}
"#;

    let (src, expr_range) = strip_selection_markers(fixture);
    let db = RefactorJavaDatabase::new([(file.clone(), src.clone())]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: "selector".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(
            err,
            SemanticRefactorError::ExtractNotSupported { reason }
                if reason == "cannot extract because it may change evaluation order"
        ),
        "expected eval-order rejection, got: {err:?}"
    );
}

#[test]
fn extract_variable_rejected_in_annotation_value() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  @SuppressWarnings("unchecked")
  void m() {}
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let expr_start = src.find("\"unchecked\"").unwrap();
    let expr_end = expr_start + "\"unchecked\"".len();

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(expr_start, expr_end),
            name: "tmp".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(err, SemanticRefactorError::ExtractNotSupported { .. }),
        "expected ExtractNotSupported, got: {err:?}"
    );
}

#[test]
fn extract_variable_rejected_in_annotation_value_nested_expression() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  @A(1 + 2)
  void m() {}
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let expr_start = src.find("2").unwrap();
    let expr_end = expr_start + "2".len();

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(expr_start, expr_end),
            name: "tmp".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(err, SemanticRefactorError::ExtractNotSupported { .. }),
        "expected ExtractNotSupported, got: {err:?}"
    );
}

#[test]
fn extract_variable_rejected_in_annotation_default_value() {
    let file = FileId::new("Test.java");
    let src = r#"@interface TestAnno {
  String value() default "unchecked";
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let expr_start = src.find("\"unchecked\"").unwrap();
    let expr_end = expr_start + "\"unchecked\"".len();

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(expr_start, expr_end),
            name: "tmp".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(err, SemanticRefactorError::ExtractNotSupported { .. }),
        "expected ExtractNotSupported, got: {err:?}"
    );
}

#[test]
fn extract_variable_rejected_in_annotation_default_value_nested_expression() {
    let file = FileId::new("Test.java");
    let src = r#"@interface TestAnno {
  int value() default 1 + 2;
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let expr_start = src.find("2").unwrap();
    let expr_end = expr_start + "2".len();

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(expr_start, expr_end),
            name: "tmp".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(err, SemanticRefactorError::ExtractNotSupported { .. }),
        "expected ExtractNotSupported, got: {err:?}"
    );
}

#[test]
fn extract_variable_rejected_in_switch_case_label() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m(int x) {
    switch (x) {
      case 1 + 2:
        break;
    }
  }
 }
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let expr_start = src.find("1 + 2").unwrap();
    let expr_end = expr_start + "1 + 2".len();

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(expr_start, expr_end),
            name: "tmp".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(err, SemanticRefactorError::ExtractNotSupported { .. }),
        "expected ExtractNotSupported, got: {err:?}"
    );
}

#[test]
fn extract_variable_rejected_in_switch_case_label_nested_expression() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m(int x) {
    switch (x) {
      case 1 + 2:
        break;
    }
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let expr_start = src.find("2").unwrap();
    let expr_end = expr_start + "2".len();

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(expr_start, expr_end),
            name: "tmp".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(err, SemanticRefactorError::ExtractNotSupported { .. }),
        "expected ExtractNotSupported, got: {err:?}"
    );
}

#[test]
fn extract_variable_rejected_in_switch_expression_case_label() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  int m(int x) {
    return switch (x) {
      case 1 + 2 -> 0;
      default -> 1;
    };
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let expr_start = src.find("1 + 2").unwrap();
    let expr_end = expr_start + "1 + 2".len();

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(expr_start, expr_end),
            name: "tmp".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(err, SemanticRefactorError::ExtractNotSupported { .. }),
        "expected ExtractNotSupported, got: {err:?}"
    );
}

#[test]
fn extract_variable_rejected_in_switch_expression_case_label_nested_expression() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  int m(int x) {
    return switch (x) {
      case 1 + /*select*/2/*end*/ -> 0;
      default -> 1;
    };
  }
}
"#;
    let (src, selection) = strip_selection_markers(src);
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: selection,
            name: "tmp".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(err, SemanticRefactorError::ExtractNotSupported { .. }),
        "expected ExtractNotSupported, got: {err:?}"
    );
}

#[test]
fn extract_variable_replace_all_does_not_cross_switch_case_groups() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m(int x) {
    switch (x) {
      case 1:
        System.out.println(1 + 2);
        System.out.println(1 + 2);
        break;
      case 2:
        System.out.println(1 + 2);
        break;
    }
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let expr_start = src.find("1 + 2").unwrap();
    let expr_end = expr_start + "1 + 2".len();

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(expr_start, expr_end),
            name: "sum".into(),
            use_var: true,
            replace_all: true,
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    let expected = r#"class Test {
  void m(int x) {
    switch (x) {
      case 1:
        var sum = 1 + 2;
        System.out.println(sum);
        System.out.println(sum);
        break;
      case 2:
        System.out.println(1 + 2);
        break;
    }
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_replace_all_does_not_cross_switch_expression_case_groups() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  int m(int x) {
    int y = switch (x) {
      case 1:
        System.out.println(1 + 2);
        System.out.println(1 + 2);
        yield 0;
      case 2:
        System.out.println(1 + 2);
        yield 1;
      default:
        yield 2;
    };
    return y;
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let expr_start = src.find("1 + 2").unwrap();
    let expr_end = expr_start + "1 + 2".len();

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(expr_start, expr_end),
            name: "sum".into(),
            use_var: true,
            replace_all: true,
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    let expected = r#"class Test {
  int m(int x) {
    int y = switch (x) {
      case 1:
        var sum = 1 + 2;
        System.out.println(sum);
        System.out.println(sum);
        yield 0;
      case 2:
        System.out.println(1 + 2);
        yield 1;
      default:
        yield 2;
    };
    return y;
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_replace_all_does_not_cross_switch_rule_blocks() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m(int x) {
    switch (x) {
      case 1 -> {
        System.out.println(1 + 2);
        System.out.println(1 + 2);
      }
      case 2 -> {
        System.out.println(1 + 2);
      }
    }
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let expr_start = src.find("1 + 2").unwrap();
    let expr_end = expr_start + "1 + 2".len();

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(expr_start, expr_end),
            name: "sum".into(),
            use_var: true,
            replace_all: true,
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    let expected = r#"class Test {
  void m(int x) {
    switch (x) {
      case 1 -> {
        var sum = 1 + 2;
        System.out.println(sum);
        System.out.println(sum);
      }
      case 2 -> {
        System.out.println(1 + 2);
      }
    }
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_replace_all_does_not_cross_switch_case_groups_with_fallthrough() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m(int x) {
    switch (x) {
      case 1:
        System.out.println(1 + 2);
      case 2:
        System.out.println(1 + 2);
        break;
    }
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let expr_start = src.find("1 + 2").unwrap();
    let expr_end = expr_start + "1 + 2".len();

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(expr_start, expr_end),
            name: "sum".into(),
            use_var: true,
            replace_all: true,
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    let expected = r#"class Test {
  void m(int x) {
    switch (x) {
      case 1:
        var sum = 1 + 2;
        System.out.println(sum);
      case 2:
        System.out.println(1 + 2);
        break;
    }
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_rejected_in_try_with_resources_resource_initializer() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    try (Foo f = /*select*/makeFoo()/*end*/) {
      use(f);
    }
  }
}
"#;
    let (src, selection) = strip_selection_markers(src);
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: selection,
            name: "tmp".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(
            err,
            SemanticRefactorError::ExtractNotSupported { reason }
                if reason == "cannot extract from try-with-resources resource specification"
        ),
        "expected ExtractNotSupported for try-with-resources, got: {err:?}"
    );
}

#[test]
fn extract_variable_rejects_expression_in_assert_condition() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    assert 1 + 2 > 0;
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let expr_start = src.find("1 + 2").unwrap();
    let expr_end = expr_start + "1 + 2".len();

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(expr_start, expr_end),
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(err, SemanticRefactorError::ExtractNotSupportedInAssert),
        "expected ExtractNotSupportedInAssert error, got: {err:?}"
    );
}

#[test]
fn extract_variable_rejects_expression_in_assert_message() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    assert true : 1 + 2;
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let expr_start = src.find("1 + 2").unwrap();
    let expr_end = expr_start + "1 + 2".len();

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: WorkspaceTextRange::new(expr_start, expr_end),
            name: "sum".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(err, SemanticRefactorError::ExtractNotSupportedInAssert),
        "expected ExtractNotSupportedInAssert error, got: {err:?}"
    );
}

#[test]
fn rename_local_variable_does_not_touch_shadowed_field() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
      int foo = 0;

  void m() {
    int foo = 1;
    System.out.println(foo + this.foo);
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("int foo = 1").unwrap() + "int ".len() + 1;
    let symbol = db.symbol_at(&file, offset).expect("symbol at local foo");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "bar".into(),
        },
    )
    .unwrap();
    let after = apply_text_edits(src, &edit.text_edits).unwrap();

    assert!(
        after.contains("int foo = 0;"),
        "field declaration should remain unchanged: {after}"
    );
    assert!(
        after.contains("int bar = 1;"),
        "local declaration should be renamed: {after}"
    );
    assert!(
        after.contains("println(bar + this.foo);"),
        "only local usage should be renamed: {after}"
    );
}

#[test]
fn rename_field_conflicts_on_local_name_capture() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  int foo = 0;

  void m() {
    int bar = 1;
    System.out.println(foo);
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("int foo").unwrap() + "int ".len() + 1;
    let symbol = db.symbol_at(&file, offset).expect("symbol at field foo");

    let err = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "bar".into(),
        },
    )
    .unwrap_err();
    let SemanticRefactorError::Conflicts(conflicts) = err else {
        panic!("expected conflicts, got: {err:?}");
    };

    assert!(
        conflicts.iter().any(|c| matches!(
            c,
            Conflict::ReferenceWillChangeResolution { name, .. } if name == "bar"
        )),
        "expected ReferenceWillChangeResolution conflict: {conflicts:?}"
    );
}

#[test]
fn rename_parameter_updates_body_references() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m(int foo) {
    System.out.println(foo);
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("int foo").unwrap() + "int ".len() + 1;
    let symbol = db
        .symbol_at(&file, offset)
        .expect("symbol at parameter foo");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "bar".into(),
        },
    )
    .unwrap();
    let after = apply_text_edits(src, &edit.text_edits).unwrap();

    assert!(
        after.contains("void m(int bar)"),
        "parameter should be renamed: {after}"
    );
    assert!(
        after.contains("println(bar);"),
        "parameter usage should be renamed: {after}"
    );
}

#[test]
fn rename_anonymous_class_constructor_parameter_updates_body_references() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  static class Base { Base(int x) {} }

  void m() {
    new Base(1) {
      Base(int foo) { System.out.println(foo); }
    };
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("int foo").unwrap() + "int ".len() + 1;
    let symbol = db
        .symbol_at(&file, offset)
        .expect("symbol at anonymous ctor parameter foo");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "bar".into(),
        },
    )
    .unwrap();
    let after = apply_text_edits(src, &edit.text_edits).unwrap();

    assert!(
        after.contains("Base(int bar)"),
        "parameter should be renamed: {after}"
    );
    assert!(
        after.contains("println(bar);"),
        "parameter usage should be renamed: {after}"
    );
    assert!(!after.contains("foo"), "expected foo to be fully renamed: {after}");
}

#[test]
fn rename_record_component_updates_header_and_references() {
    let file = FileId::new("Test.java");
    let src = r#"record P(int x) {
  P { System.out.println(x); }
  int f() { return x; }
  int g() { return x(); }
  int h() { return this.x(); }
 }

class Use {
  void m() {
    P p = null;
    p.x();
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("record P(int x").unwrap() + "record P(int ".len();
    let symbol = db
        .symbol_at(&file, offset)
        .expect("symbol at record component x");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "y".into(),
        },
    )
    .unwrap();
    let after = apply_text_edits(src, &edit.text_edits).unwrap();

    assert!(
        after.contains("record P(int y)"),
        "record header component should be renamed: {after}"
    );
    assert!(
        after.contains("return y;"),
        "record body references should be renamed: {after}"
    );
    assert!(
        after.contains("System.out.println(y);"),
        "compact constructor param should be renamed: {after}"
    );
    assert!(
        after.contains("return y();"),
        "record body accessor calls should be renamed: {after}"
    );
    assert!(
        after.contains("return this.y();"),
        "record body qualified accessor calls should be renamed: {after}"
    );
    assert!(
        after.contains("p.y();"),
        "external accessor calls should be renamed: {after}"
    );
}

#[test]
fn rename_record_component_updates_explicit_canonical_constructor_params() {
    let file = FileId::new("Test.java");
    let src = r#"record P(int x) {
  P(int x) { System.out.println(x); }
  int f() { return x; }
 }
 
 class Use {
   void m() {
     P p = null;
     p.x();
   }
 }
 "#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("record P(int x").unwrap() + "record P(int ".len();
    let symbol = db
        .symbol_at(&file, offset)
        .expect("symbol at record component x");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "y".into(),
        },
    )
    .unwrap();
    let after = apply_text_edits(src, &edit.text_edits).unwrap();

    assert!(
        after.contains("record P(int y)"),
        "record header component should be renamed: {after}"
    );
    assert!(
        after.contains("P(int y) { System.out.println(y); }"),
        "canonical constructor parameter should be renamed: {after}"
    );
    assert!(
        after.contains("return y;"),
        "record body references should be renamed: {after}"
    );
    assert!(
        after.contains("p.y();"),
        "external accessor calls should be renamed: {after}"
    );
}

#[test]
fn rename_local_variable_does_not_touch_type_arguments_or_annotations() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  @interface Foo {}

  void m() {
    int Foo = 1;
    java.util.List<Foo> xs = null;
    @Foo int y = Foo;
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("int Foo = 1").unwrap() + "int ".len() + 1;
    let symbol = db.symbol_at(&file, offset).expect("symbol at local Foo");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "Bar".into(),
        },
    )
    .unwrap();
    let after = apply_text_edits(src, &edit.text_edits).unwrap();

    assert!(
        after.contains("java.util.List<Foo>"),
        "type argument should remain unchanged: {after}"
    );
    assert!(
        after.contains("@Foo int y"),
        "annotation should remain unchanged: {after}"
    );
    assert!(
        after.contains("int Bar = 1;"),
        "local declaration should be renamed: {after}"
    );
    assert!(
        after.contains("y = Bar;"),
        "local usage should be renamed: {after}"
    );
}

#[test]
fn rename_updates_annotation_method_default_enum_constant() {
    let file = FileId::new("Test.java");
    let src = r#"enum E { FOO, BAR }
@interface A { E v() default E.FOO; }
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    // Pick the enum constant declaration, not the default value usage.
    let offset = src.find("FOO,").unwrap() + 1;
    let symbol = db.symbol_at(&file, offset).expect("symbol at FOO");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "BAZ".into(),
        },
    )
    .unwrap();
    let after = apply_text_edits(src, &edit.text_edits).unwrap();

    assert!(after.contains("enum E { BAZ, BAR }"), "{after}");
    assert!(after.contains("default E.BAZ;"), "{after}");
}

#[test]
fn rename_updates_annotation_method_default_class_literal() {
    let file = FileId::new("Test.java");
    let src = r#"@interface A { Class<?> c() default Foo.class; }
class Foo {}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    // Pick the class declaration, not the class literal usage.
    let offset = src.find("class Foo").unwrap() + "class ".len() + 1;
    let symbol = db.symbol_at(&file, offset).expect("symbol at Foo");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "Bar".into(),
        },
    )
    .unwrap();
    let after = apply_text_edits(src, &edit.text_edits).unwrap();

    assert!(after.contains("default Bar.class;"), "{after}");
    assert!(after.contains("class Bar {}"), "{after}");
    assert!(!after.contains("Foo"), "{after}");
}

#[test]
fn rename_shadowing_conflict_detected_in_nested_block_scope() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int outer = 1;
    {
      int inner = outer + 1;
    }
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("int inner").unwrap() + "int ".len() + 1;
    let symbol = db.symbol_at(&file, offset).expect("symbol at inner");

    let err = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "outer".into(),
        },
    )
    .unwrap_err();
    let SemanticRefactorError::Conflicts(conflicts) = err else {
        panic!("expected conflicts, got: {err:?}");
    };

    assert!(
        conflicts
            .iter()
            .any(|c| matches!(c, Conflict::Shadowing { name, .. } if name == "outer")),
        "expected Shadowing conflict: {conflicts:?}"
    );
}

#[test]
fn rename_outer_local_conflict_with_inner_block_local_is_rejected() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int outer = 1;
    {
      int inner = 2;
    }
    System.out.println(outer);
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("int outer").unwrap() + "int ".len() + 1;
    let symbol = db.symbol_at(&file, offset).expect("symbol at outer");

    let err = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "inner".into(),
        },
    )
    .unwrap_err();
    let SemanticRefactorError::Conflicts(conflicts) = err else {
        panic!("expected conflicts, got: {err:?}");
    };

    assert!(
        conflicts
            .iter()
            .any(|c| matches!(c, Conflict::NameCollision { name, .. } if name == "inner")),
        "expected NameCollision conflict: {conflicts:?}"
    );
}

#[test]
fn rename_type_from_constructor_declaration_renames_constructors() {
    let file = FileId::new("Test.java");
    let src = r#"class Foo {
  Foo() {}
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("Foo()").unwrap() + 1;
    let symbol = db
        .symbol_at(&file, offset)
        .expect("symbol at constructor name");
    assert_eq!(
        db.symbol_kind(symbol),
        Some(nova_refactor::JavaSymbolKind::Type)
    );

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "Bar".into(),
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();

    assert!(
        after.contains("class Bar"),
        "expected class name to be renamed: {after}"
    );
    assert!(
        after.contains("Bar()"),
        "expected constructor name to be renamed: {after}"
    );
    assert!(
        !after.contains("Foo"),
        "expected Foo to be fully renamed: {after}"
    );
}

#[test]
fn rename_method_updates_super_method_reference() {
    let file = FileId::new("Test.java");
    let src = r#"class Base { void m(){} }
class Derived extends Base { java.util.function.Supplier<?> s = super::m; }
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("void m").unwrap() + "void ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at method m");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "n".into(),
        },
    )
    .unwrap();
    let after = apply_text_edits(src, &edit.text_edits).unwrap();

    assert!(
        after.contains("void n()"),
        "expected method to be renamed: {after}"
    );
    assert!(
        after.contains("super::n"),
        "expected super method reference to be renamed: {after}"
    );
}

#[test]
fn rename_type_updates_expression_level_type_positions() {
    let file = FileId::new("Test.java");
    let src = r#"class Foo {}

class Test {
  <T> void m() {}

  void use(Object x) {
    Object y = (Foo) x;
    boolean b = x instanceof Foo;
    Foo[] a = new Foo[1];
    this.<Foo>m();
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("class Foo").unwrap() + "class ".len() + 1;
    let symbol = db.symbol_at(&file, offset).expect("symbol at Foo");
    assert_eq!(
        db.symbol_kind(symbol),
        Some(nova_refactor::JavaSymbolKind::Type)
    );

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "Bar".into(),
        },
    )
    .unwrap();
    let after = apply_text_edits(src, &edit.text_edits).unwrap();

    assert!(
        after.contains("class Bar"),
        "type declaration should be renamed: {after}"
    );
    assert!(
        after.contains("Object y = (Bar) x;"),
        "cast type should be renamed: {after}"
    );
    assert!(
        after.contains("x instanceof Bar"),
        "instanceof type should be renamed: {after}"
    );
    assert!(
        after.contains("Bar[] a = new Bar[1];"),
        "array creation type should be renamed: {after}"
    );
    assert!(
        after.contains("this.<Bar>m();"),
        "explicit generic invocation type args should be renamed: {after}"
    );
}

#[test]
fn rename_type_does_not_touch_comments_inside_explicit_type_arguments() {
    let file = FileId::new("Test.java");
    let src = r#"class Foo {}

class Test {
  <T> void m() {}

  void use() {
    this.</*Foo*/Foo>m();
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("class Foo").unwrap() + "class ".len() + 1;
    let symbol = db.symbol_at(&file, offset).expect("symbol at Foo");
    assert_eq!(
        db.symbol_kind(symbol),
        Some(nova_refactor::JavaSymbolKind::Type)
    );

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "Bar".into(),
        },
    )
    .unwrap();
    let after = apply_text_edits(src, &edit.text_edits).unwrap();

    assert!(
        after.contains("this.</*Foo*/Bar>m();"),
        "expected type argument to be renamed but comment preserved: {after}"
    );
    assert!(
        after.contains("/*Foo*/"),
        "expected comment contents to remain unchanged: {after}"
    );
    assert!(
        !after.contains("/*Bar*/"),
        "expected rename to not update comment contents: {after}"
    );
}

#[test]
fn rename_type_updates_nested_type_qualifiers_in_expression_level_type_positions() {
    let file = FileId::new("Test.java");
    let src = r#"class Outer {
  static class Inner {}
}

class Use {
  <T> void m() {}

  void f(Object x) {
    Object y = (Outer.Inner) x;
    boolean b = x instanceof Outer.Inner;
    Outer.Inner[] a = new Outer.Inner[1];
    new Outer.Inner[1].getClass();
    this.<Outer.Inner>m();
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("class Outer").unwrap() + "class ".len() + 1;
    let symbol = db.symbol_at(&file, offset).expect("symbol at Outer");
    assert_eq!(db.symbol_kind(symbol), Some(JavaSymbolKind::Type));

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "Renamed".into(),
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    assert!(after.contains("class Renamed"));
    assert!(after.contains("(Renamed.Inner)"), "{after}");
    assert!(after.contains("instanceof Renamed.Inner"), "{after}");
    assert!(
        after.contains("Renamed.Inner[] a = new Renamed.Inner[1];"),
        "{after}"
    );
    assert!(
        after.contains("new Renamed.Inner[1].getClass();"),
        "{after}"
    );
    assert!(after.contains("this.<Renamed.Inner>m();"), "{after}");
    assert!(!after.contains("Outer.Inner"), "{after}");
}

#[test]
fn rename_type_updates_explicit_generic_invocation_static_receiver_and_type_args() {
    let file = FileId::new("Test.java");
    let src = r#"class Foo {
  static <T> void id() {}
}

class Use {
  void f() {
    Foo.<Foo>id();
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("class Foo").unwrap() + "class ".len() + 1;
    let symbol = db.symbol_at(&file, offset).expect("symbol at Foo");
    assert_eq!(db.symbol_kind(symbol), Some(JavaSymbolKind::Type));

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "Bar".into(),
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    assert!(after.contains("class Bar"), "{after}");
    assert!(after.contains("Bar.<Bar>id();"), "{after}");
    assert!(!after.contains("Foo.<Foo>id();"), "{after}");
}

#[test]
fn inline_variable_all_usages_replaces_and_deletes_declaration() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int a = 1 + 2;
    System.out.println(a);
    System.out.println(a);
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let edit = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    let expected = r#"class Test {
  void m() {
    System.out.println((1 + 2));
    System.out.println((1 + 2));
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn inline_variable_preserves_crlf_newlines_and_removes_decl_cleanly() {
    let file = FileId::new("Test.java");
    let src_lf = r#"class Test {
  void m() {
    int a = 1 + 2;
    System.out.println(a);
    System.out.println(a);
  }
}
"#;
    let src = to_crlf(src_lf);
    let db = RefactorJavaDatabase::new([(file.clone(), src.clone())]);

    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let edit = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap();

    let after = apply_text_edits(&src, &edit.text_edits).unwrap();
    assert_all_newlines_are_crlf(&after);

    let expected_lf = r#"class Test {
  void m() {
    System.out.println((1 + 2));
    System.out.println((1 + 2));
  }
}
"#;
    let expected = to_crlf(expected_lf);
    assert_eq!(after, expected);
}

#[test]
fn inline_variable_preserves_no_final_newline() {
    let file = FileId::new("Test.java");
    let src = "class Test {
  void m() {
    int a = 1 + 2;
    System.out.println(a);
    System.out.println(a);
  }
}";
    assert!(
        !src.ends_with('\n') && !src.ends_with('\r'),
        "test precondition: fixture must not end with a newline"
    );

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let edit = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    assert!(
        !after.ends_with('\n') && !after.ends_with('\r'),
        "expected refactoring to preserve lack of final newline, got: {after:?}"
    );
    let expected = "class Test {
  void m() {
    System.out.println((1 + 2));
    System.out.println((1 + 2));
  }
}";
    assert_eq!(after, expected);
}

#[test]
fn inline_variable_inline_all_rejected_when_unindexed_occurrence_exists() {
    let file = FileId::new("Test.java");
    let src = r#"class C {
  void m() {
    int a = 1 + 2;
    Runnable r = new Runnable() {
      public void run() {
        System.out.println(a);
      }
    };
    System.out.println(a);
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    // `inline_all` must not delete the declaration if we cannot prove all occurrences were indexed.
    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap_err();
    assert!(matches!(err, SemanticRefactorError::InlineNotSupported));

    // The non-deleting variant stays supported.
    let mut refs = db.find_references(symbol);
    refs.sort_by_key(|r| r.range.start);
    let usage = refs.first().expect("at least one indexed reference").range;

    let edit = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: false,
            usage_range: Some(usage),
        },
    )
    .unwrap();
    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    assert!(after.contains("int a = 1 + 2;"), "declaration must remain");
    assert!(
        after.contains("System.out.println((1 + 2));"),
        "at least one usage should be inlined: {after}"
    );
}

#[test]
fn inline_variable_inline_all_rejected_when_unindexed_qualified_occurrence_exists() {
    let file = FileId::new("Test.java");
    let src = r#"class C {
  void m() {
    String a = "hi";
    Runnable r = new Runnable() {
      public void run() {
        System.out.println(a.length());
      }
    };
    System.out.println(a);
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("String a").unwrap() + "String ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap_err();
    assert!(matches!(err, SemanticRefactorError::InlineNotSupported));

    // The non-deleting variant stays supported, even though we cannot delete the declaration.
    let mut refs = db.find_references(symbol);
    refs.sort_by_key(|r| r.range.start);
    let usage = refs.first().expect("at least one indexed reference").range;

    let edit = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: false,
            usage_range: Some(usage),
        },
    )
    .unwrap();
    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    assert!(
        after.contains("String a = \"hi\";"),
        "declaration must remain"
    );
    assert!(
        after.contains("System.out.println(\"hi\");"),
        "selected usage should be inlined: {after}"
    );
    assert!(
        after.contains("a.length()"),
        "unindexed occurrence must remain untouched: {after}"
    );
}

#[test]
fn inline_variable_all_usages_succeeds_when_only_usage_is_qualified() {
    let file = FileId::new("Test.java");
    let src = r#"class C {
  void m() {
    String a = "hi";
    System.out.println(a.length());
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("String a").unwrap() + "String ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let edit = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    assert!(
        !after.contains("String a"),
        "expected declaration to be removed: {after}"
    );
    assert!(
        after.contains("System.out.println(\"hi\".length());"),
        "expected qualified usage to be inlined: {after}"
    );
}

#[test]
fn inline_variable_all_usages_ignores_unindexed_method_call_named_like_variable() {
    // Some syntax occurrences of the variable name are not variable references (e.g. method-call
    // callees). In unindexed contexts (anonymous class bodies), `symbol_at` can fail for these too,
    // so the "unknown occurrence" scan must avoid false positives that would incorrectly reject
    // `inline_all`.
    let file = FileId::new("Test.java");
    let src = r#"class C {
  void m() {
    int a = 1 + 2;
    Runnable r = new Runnable() {
      public void run() {
        a();
      }
      void a() {}
    };
    System.out.println(a);
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let edit = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    assert!(
        !after.contains("int a"),
        "expected declaration to be removed: {after}"
    );
    assert!(
        after.contains("System.out.println((1 + 2));"),
        "expected usage to be inlined: {after}"
    );
    assert!(
        after.contains("a();"),
        "expected unrelated method call to remain: {after}"
    );
}

#[test]
fn inline_variable_inline_one_rejected_when_decl_cannot_be_removed_and_initializer_has_side_effects(
) {
    // If `find_references` does not report all textual occurrences, `inline_all=false` must keep the
    // declaration. In that case, inlining a side-effectful initializer would duplicate evaluation,
    // so the refactoring must be rejected.
    let file = FileId::new("Test.java");
    let src = r#"class C {
  int foo() { return 1; }
  void m() {
    int a = foo();
    Runnable r = new Runnable() {
      public void run() {
        System.out.println(a);
      }
    };
    System.out.println(a);
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let mut refs = db.find_references(symbol);
    refs.sort_by_key(|r| r.range.start);
    let usage = refs.first().expect("at least one indexed reference").range;

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: false,
            usage_range: Some(usage),
        },
    )
    .unwrap_err();
    assert!(
        matches!(err, SemanticRefactorError::InlineSideEffects),
        "expected InlineSideEffects, got: {err:?}"
    );
}

#[test]
fn inline_variable_rejects_array_access_with_intervening_statement() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m(int[] arr, int i) {
    int a = arr[i];
    foo();
    System.out.println(a);
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap_err();

    assert!(
        matches!(err, SemanticRefactorError::InlineNotSupported),
        "expected InlineNotSupported, got {err:?}"
    );
}

#[test]
fn inline_variable_allows_array_access_when_usage_is_next_statement() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m(int[] arr, int i) {
    int a = arr[i];
    System.out.println(a);
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let edit = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap();
    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    let expected = r#"class Test {
  void m(int[] arr, int i) {
    System.out.println(arr[i]);
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn inline_variable_rejects_cast_with_intervening_statement() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m(Object o) {
    String s = (String) o;
    foo();
    System.out.println(s);
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("String s").unwrap() + "String ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at s");

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap_err();
    assert!(
        matches!(err, SemanticRefactorError::InlineNotSupported),
        "expected InlineNotSupported, got {err:?}"
    );
}

#[test]
fn inline_variable_rejected_when_initializer_dependency_is_written_before_use() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int x = 1;
    int a = x;
    x = 2;
    System.out.println(a);
    System.out.println(a);
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap_err();

    assert!(
        matches!(err, SemanticRefactorError::InlineWouldChangeValue { .. }),
        "expected InlineWouldChangeValue, got: {err:?}"
    );
}

#[test]
fn inline_variable_rejected_when_initializer_dependency_is_written_in_later_declarator_same_statement(
) {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int x = 1, a = x, y = (x = 2);
    System.out.println(a);
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("a = x").unwrap();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap_err();

    assert!(
        matches!(err, SemanticRefactorError::InlineWouldChangeValue { .. }),
        "expected InlineWouldChangeValue, got: {err:?}"
    );
}

#[test]
fn inline_variable_rejected_when_initializer_field_dependency_is_written_before_use() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  int x = 1;
  void m() {
    int a = x;
    x = 2;
    System.out.println(a);
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap_err();

    assert!(
        matches!(err, SemanticRefactorError::InlineWouldChangeValue { .. }),
        "expected InlineWouldChangeValue, got: {err:?}"
    );
}

#[test]
fn inline_variable_rejected_when_initializer_dependency_is_written_before_use_inline_one() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int x = 1;
    int a = x;
    x = 2;
    System.out.println(a);
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let mut refs = db.find_references(symbol);
    refs.sort_by_key(|r| r.range.start);
    assert_eq!(refs.len(), 1, "expected one reference");
    let usage = refs[0].range;

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: false,
            usage_range: Some(usage),
        },
    )
    .unwrap_err();

    assert!(
        matches!(err, SemanticRefactorError::InlineWouldChangeValue { .. }),
        "expected InlineWouldChangeValue, got: {err:?}"
    );
}

#[test]
fn inline_variable_allowed_when_initializer_dependencies_are_not_written() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int x = 1;
    int a = x;
    System.out.println(a);
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let edit = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    let expected = r#"class Test {
  void m() {
    int x = 1;
    System.out.println(x);
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn inline_variable_rejected_when_initializer_dependency_is_written_inside_enclosing_loop() {
    let file = FileId::new("Test.java");
    let src = r#"class C {
  void m(boolean cond) {
    int x = 0;
    int a = x;
    while (cond) {
      System.out.println(a);
      x++;
    }
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap_err();
    assert!(
        matches!(err, SemanticRefactorError::InlineNotSupported),
        "expected InlineNotSupported, got: {err:?}"
    );
}

#[test]
fn inline_variable_allowed_when_initializer_dependency_not_written_inside_enclosing_loop() {
    let file = FileId::new("Test.java");
    let src = r#"class C {
  void m(boolean cond) {
    int x = 0;
    int a = x;
    while (cond) {
      System.out.println(a);
    }
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let edit = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    let expected = r#"class C {
  void m(boolean cond) {
    int x = 0;
    while (cond) {
      System.out.println(x);
    }
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn inline_variable_wraps_cast_receiver() {
    let file = FileId::new("Test.java");
    let src = r#"class Foo { void m() {} }
class C {
  void t(Object o) {
    Foo a = (Foo) o;
    a.m();
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("Foo a").unwrap() + "Foo ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let edit = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    let expected = r#"class Foo { void m() {} }
class C {
  void t(Object o) {
    ((Foo) o).m();
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn inline_variable_wraps_conditional_receiver() {
    let file = FileId::new("Test.java");
    let src = r#"class Foo { void m() {} }
class C {
  void t(boolean cond, Foo x, Foo y) {
    Foo a = cond ? x : y;
    a.m();
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("Foo a").unwrap() + "Foo ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let edit = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    let expected = r#"class Foo { void m() {} }
class C {
  void t(boolean cond, Foo x, Foo y) {
    (cond ? x : y).m();
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn inline_variable_does_not_parenthesize_method_call_receiver() {
    let file = FileId::new("Test.java");
    let src = r#"class Foo { void m() {} }
class C {
  Foo make() { return null; }
  void t() {
    Foo a = make();
    a.m();
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("Foo a").unwrap() + "Foo ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let edit = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    let expected = r#"class Foo { void m() {} }
class C {
  Foo make() { return null; }
  void t() {
    make().m();
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn inline_variable_multi_declarator_statement_removes_first_declarator() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int a = 1, b = a + 1;
    System.out.println(b);
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let edit = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    let expected = r#"class Test {
  void m() {
    int b = 1 + 1;
    System.out.println(b);
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn inline_variable_multi_declarator_statement_removes_middle_declarator() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int a = 1, b = a + 1, c = b + 1;
    System.out.println(c);
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("b =").unwrap();
    let symbol = db.symbol_at(&file, offset).expect("symbol at b");

    let edit = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    let expected = r#"class Test {
  void m() {
    int a = 1, c = (a + 1) + 1;
    System.out.println(c);
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn inline_variable_multi_declarator_statement_removes_last_declarator() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int a = 1, b = 2, c = b + 1;
    System.out.println(c);
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("c =").unwrap();
    let symbol = db.symbol_at(&file, offset).expect("symbol at c");

    let edit = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    let expected = r#"class Test {
  void m() {
    int a = 1, b = 2;
    System.out.println((b + 1));
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn inline_variable_multi_declarator_inline_one_keeps_declaration() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int a = 1, b = 2;
    System.out.println(a);
    System.out.println(a);
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let mut refs = db.find_references(symbol);
    refs.sort_by_key(|r| r.range.start);
    assert_eq!(refs.len(), 2, "expected two references");
    let first_usage = refs[0].range;

    let edit = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: false,
            usage_range: Some(first_usage),
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    let expected = r#"class Test {
  void m() {
    int a = 1, b = 2;
    System.out.println(1);
    System.out.println(a);
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn inline_variable_multi_declarator_side_effects_in_other_initializer_is_rejected() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  int foo() { return 1; }
  int bar() { return 2; }
  void m() {
    int a = foo(), b = bar();
    System.out.println(a);
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap_err();
    assert!(
        matches!(err, SemanticRefactorError::InlineSideEffects),
        "expected InlineSideEffects, got: {err:?}"
    );
}

#[test]
fn inline_variable_in_switch_one_line_case_label_does_not_delete_case() {
    let file = FileId::new("Test.java");
    let src = r#"class C {
  void m(int x) {
    switch (x) {
      case 1: int a = 1 + 2; System.out.println(a); break;
    }
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let edit = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap();
    let after = apply_text_edits(src, &edit.text_edits).unwrap();

    let expected = r#"class C {
  void m(int x) {
    switch (x) {
      case 1: System.out.println((1 + 2)); break;
    }
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn inline_variable_in_switch_case_label_declaration_at_eol_deletes_newline() {
    // Regression test for the mid-line declaration deletion path when the declaration ends the line
    // (`stmt_end` is immediately followed by a newline). The declaration should be removed without
    // deleting `case 1:`, and the following statement should be kept.
    let file = FileId::new("Test.java");
    let src = r#"class C {
  void m(int x) {
    switch (x) {
      case 1: int a = 1 + 2;
System.out.println(a); break;
    }
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let edit = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap();
    let after = apply_text_edits(src, &edit.text_edits).unwrap();

    let expected = r#"class C {
  void m(int x) {
    switch (x) {
      case 1: System.out.println((1 + 2)); break;
    }
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn inline_variable_in_switch_case_label_declaration_at_eol_deletes_newline_crlf() {
    let file = FileId::new("Test.java");
    let src = "class C {\r\n  void m(int x) {\r\n    switch (x) {\r\n      case 1: int a = 1 + 2;\r\nSystem.out.println(a); break;\r\n    }\r\n  }\r\n}\r\n";

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let edit = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap();
    let after = apply_text_edits(src, &edit.text_edits).unwrap();

    let expected =
        "class C {\r\n  void m(int x) {\r\n    switch (x) {\r\n      case 1: System.out.println((1 + 2)); break;\r\n    }\r\n  }\r\n}\r\n";
    assert_eq!(after, expected);
}

#[test]
fn inline_variable_mid_line_switch_case_declaration_removes_line_comment() {
    let file = FileId::new("C.java");
    let src = r#"class C {
  void m(int x) {
    switch (x) {
      case 1: int a = 1 + 2; // temp
              System.out.println(a);
    }
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let edit = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap();
    let after = apply_text_edits(src, &edit.text_edits).unwrap();

    assert!(
        !after.contains("// temp"),
        "expected trailing line comment to be deleted with declaration: {after}"
    );
    assert!(
        after.contains("case 1:"),
        "expected switch case label to remain after inlining: {after}"
    );
    assert!(
        after.contains("System.out.println((1 + 2));"),
        "expected initializer to be inlined into usage: {after}"
    );
}

#[test]
fn inline_variable_mid_line_switch_case_declaration_removes_block_comment() {
    let file = FileId::new("C.java");
    let src = r#"class C {
  void m(int x) {
    switch (x) {
      case 1: int a = 1 + 2; /* temp */
              System.out.println(a);
    }
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let edit = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap();
    let after = apply_text_edits(src, &edit.text_edits).unwrap();

    assert!(
        !after.contains("/* temp */"),
        "expected trailing block comment to be deleted with declaration: {after}"
    );
    assert!(
        after.contains("case 1:"),
        "expected switch case label to remain after inlining: {after}"
    );
    assert!(
        after.contains("System.out.println((1 + 2));"),
        "expected initializer to be inlined into usage: {after}"
    );
}

#[test]
fn inline_variable_in_switch_case_with_declaration_on_own_line_deletes_indent_cleanly() {
    let file = FileId::new("Test.java");
    let src = r#"class C {
  void m(int x) {
    switch (x) {
      case 1: {
        int a = 1 + 2;
        System.out.println(a);
        break;
      }
    }
  }
 }
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let edit = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap();
    let after = apply_text_edits(src, &edit.text_edits).unwrap();

    let expected = r#"class C {
  void m(int x) {
    switch (x) {
      case 1: {
        System.out.println((1 + 2));
        break;
      }
    }
  }
 }
"#;
    assert_eq!(after, expected);
}

#[test]
fn inline_variable_single_usage_replaces_only_selected_use() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int a = 1 + 2;
    System.out.println(a);
    System.out.println(a);
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let mut refs = db.find_references(symbol);
    refs.sort_by_key(|r| r.range.start);
    assert_eq!(refs.len(), 2, "expected two references");
    let first_usage = refs[0].range;

    let edit = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: false,
            usage_range: Some(first_usage),
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    let expected = r#"class Test {
  void m() {
    int a = 1 + 2;
    System.out.println((1 + 2));
    System.out.println(a);
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn inline_variable_inline_one_single_use_side_effectful_initializer_deletes_decl() {
    // Policy: allow inline-one when the declaration can be removed after inlining (single usage),
    // even if the initializer has side effects. This preserves "evaluate exactly once" semantics.
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  int compute() { return 1; }
  void m() {
    int x = compute();
    System.out.println(x);
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int x").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at x");

    let mut refs = db.find_references(symbol);
    refs.sort_by_key(|r| r.range.start);
    assert_eq!(refs.len(), 1, "expected one reference");
    let usage = refs[0].range;

    let edit = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: false,
            usage_range: Some(usage),
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    let expected = r#"class Test {
  int compute() { return 1; }
  void m() {
    System.out.println(compute());
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn inline_variable_single_use_side_effectful_initializer_in_if_is_rejected() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  int compute() { return 1; }
  void m(boolean cond) {
    int x = compute();
    if (cond) System.out.println(x);
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int x").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at x");

    let mut refs = db.find_references(symbol);
    refs.sort_by_key(|r| r.range.start);
    assert_eq!(refs.len(), 1, "expected one reference");
    let usage = refs[0].range;

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: false,
            usage_range: Some(usage),
        },
    )
    .unwrap_err();
    assert!(
        matches!(err, SemanticRefactorError::InlineSideEffects),
        "expected InlineSideEffects, got: {err:?}"
    );
}

#[test]
fn inline_variable_single_use_side_effectful_initializer_in_loop_is_rejected() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  int compute() { return 1; }
  void m(boolean cond) {
    int x = compute();
    while (cond) System.out.println(x);
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int x").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at x");

    let mut refs = db.find_references(symbol);
    refs.sort_by_key(|r| r.range.start);
    assert_eq!(refs.len(), 1, "expected one reference");
    let usage = refs[0].range;

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: false,
            usage_range: Some(usage),
        },
    )
    .unwrap_err();
    assert!(matches!(err, SemanticRefactorError::InlineSideEffects));
}

#[test]
fn inline_variable_single_use_side_effectful_initializer_with_intervening_statement_is_rejected() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  int compute() { return 1; }
  void side() {}
  void m() {
    int x = compute();
    side();
    System.out.println(x);
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int x").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at x");

    let mut refs = db.find_references(symbol);
    refs.sort_by_key(|r| r.range.start);
    assert_eq!(refs.len(), 1, "expected one reference");
    let usage = refs[0].range;

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: false,
            usage_range: Some(usage),
        },
    )
    .unwrap_err();
    assert!(matches!(err, SemanticRefactorError::InlineSideEffects));
}

#[test]
fn inline_variable_rejects_side_effectful_initializer_when_usage_reorders_other_side_effects() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  int foo() { return 1; }
  int bar() { return 2; }
  void m() {
    int a = foo();
    System.out.println(bar() + a);
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap_err();

    assert!(
        matches!(err, SemanticRefactorError::InlineSideEffects),
        "expected InlineSideEffects, got: {err:?}"
    );
}

#[test]
fn inline_variable_rejects_side_effectful_initializer_when_usage_is_after_other_call_argument() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  int foo() { return 1; }
  int bar() { return 2; }
  void take(int x, int y) {}
  void m() {
    int a = foo();
    take(bar(), a);
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap_err();

    assert!(
        matches!(err, SemanticRefactorError::InlineSideEffects),
        "expected InlineSideEffects, got: {err:?}"
    );
}

#[test]
fn inline_variable_rejects_side_effectful_initializer_when_usage_receiver_has_side_effects() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  int foo() { return 1; }
  Test bar() { return this; }
  void take(int x) {}
  void m() {
    int a = foo();
    bar().take(a);
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap_err();

    assert!(
        matches!(err, SemanticRefactorError::InlineSideEffects),
        "expected InlineSideEffects, got: {err:?}"
    );
}

#[test]
fn inline_variable_rejects_side_effectful_initializer_when_usage_is_conditionally_evaluated() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  int foo() { return 1; }
  int m(boolean cond) {
    int a = foo();
    return cond ? 0 : a;
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap_err();

    assert!(
        matches!(
            err,
            SemanticRefactorError::InlineNotSupported | SemanticRefactorError::InlineSideEffects
        ),
        "expected InlineNotSupported/InlineSideEffects, got: {err:?}"
    );
}

#[test]
fn inline_variable_rejects_side_effectful_initializer_in_short_circuit_rhs() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  int foo() { return 1; }
  boolean m(boolean cond) {
    int a = foo();
    return cond && a > 0;
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap_err();

    assert!(
        matches!(
            err,
            SemanticRefactorError::InlineNotSupported | SemanticRefactorError::InlineSideEffects
        ),
        "expected InlineNotSupported/InlineSideEffects, got: {err:?}"
    );
}

#[test]
fn inline_variable_rejects_side_effectful_initializer_in_while_condition() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  int foo() { return 1; }
  void m() {
    int a = foo();
    while (a < 10) {
      break;
    }
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap_err();

    assert!(
        matches!(
            err,
            SemanticRefactorError::InlineNotSupported | SemanticRefactorError::InlineSideEffects
        ),
        "expected InlineNotSupported/InlineSideEffects, got: {err:?}"
    );
}

#[test]
fn inline_variable_rejects_side_effectful_initializer_in_for_condition() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  int foo() { return 1; }
  void m() {
    int a = foo();
    for (int i = 0; i < a; i++) {
      break;
    }
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap_err();

    assert!(
        matches!(
            err,
            SemanticRefactorError::InlineNotSupported | SemanticRefactorError::InlineSideEffects
        ),
        "expected InlineNotSupported/InlineSideEffects, got: {err:?}"
    );
}

#[test]
fn inline_variable_inline_one_multi_use_side_effectful_initializer_is_rejected() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  int compute() { return 1; }
  void m() {
    int x = compute();
    System.out.println(x);
    System.out.println(x);
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int x").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at x");

    let mut refs = db.find_references(symbol);
    refs.sort_by_key(|r| r.range.start);
    assert_eq!(refs.len(), 2, "expected two references");
    let first_usage = refs[0].range;

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: false,
            usage_range: Some(first_usage),
        },
    )
    .unwrap_err();
    assert!(matches!(err, SemanticRefactorError::InlineSideEffects));
}

#[test]
fn inline_variable_side_effectful_initializer_is_rejected() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  int foo() { return 1; }
  void m() {
    int a = foo();
    System.out.println(a);
    System.out.println(a);
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap_err();
    assert!(matches!(err, SemanticRefactorError::InlineSideEffects));
}

#[test]
fn inline_variable_rejects_short_circuit_rhs_when_initializer_is_order_sensitive() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    boolean a = foo();
    if (bar() && a) {}
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("boolean a").unwrap() + "boolean ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap_err();
    assert!(matches!(err, SemanticRefactorError::InlineNotSupported));
}

#[test]
fn inline_variable_rejects_ternary_branch_when_initializer_is_order_sensitive() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m(boolean cond) {
    int a = foo();
    int x = cond ? a : 0;
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap_err();
    assert!(matches!(err, SemanticRefactorError::InlineNotSupported));
}

#[test]
fn inline_variable_rejects_while_condition_when_initializer_is_order_sensitive() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    boolean a = foo();
    while (a) {
    }
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("boolean a").unwrap() + "boolean ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap_err();
    assert!(matches!(err, SemanticRefactorError::InlineNotSupported));
}

#[test]
fn inline_variable_rejects_assert_when_initializer_is_order_sensitive() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    boolean a = foo();
    assert a;
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("boolean a").unwrap() + "boolean ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap_err();
    assert!(matches!(
        err,
        SemanticRefactorError::InlineNotSupportedInAssert
    ));
}

#[test]
fn inline_variable_rejects_switch_expression_rule_expression_body_when_initializer_is_order_sensitive(
) {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  int m(int x) {
    int a = foo();
    return switch (x) {
      case 1 -> a;
      default -> 0;
    };
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap_err();
    assert!(matches!(err, SemanticRefactorError::InlineNotSupported));
}

#[test]
fn inline_variable_all_usages_rejected_when_only_usage_is_in_assert() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  int foo() { return 1; }
  void m() {
    int x = foo();
    assert x > 0;
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int x").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at x");

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap_err();
    assert!(matches!(
        err,
        SemanticRefactorError::InlineNotSupportedInAssert
    ));
}

#[test]
fn inline_variable_inline_one_rejected_when_selected_usage_is_in_assert() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int x = 1 + 2;
    assert x > 0;
    System.out.println(x);
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int x").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at x");

    let mut refs = db.find_references(symbol);
    refs.sort_by_key(|r| r.range.start);
    assert_eq!(refs.len(), 2, "expected two references");

    let assert_start = src.find("assert x").unwrap() + "assert ".len();
    let assert_usage = refs
        .iter()
        .find(|r| r.range.start == assert_start)
        .expect("assert usage")
        .range;

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: false,
            usage_range: Some(assert_usage),
        },
    )
    .unwrap_err();
    assert!(matches!(
        err,
        SemanticRefactorError::InlineNotSupportedInAssert
    ));
}

#[test]
fn inline_variable_array_initializer_is_not_supported() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int[] xs = {1, 2};
    System.out.println(xs.length);
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int[] xs").unwrap() + "int[] ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at xs");

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap_err();
    assert!(matches!(err, SemanticRefactorError::InlineNotSupported));
}

#[test]
fn inline_variable_for_init_declaration_is_not_supported() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    for (int a = 1; a < 2; a++) {
      System.out.println(a);
    }
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap_err();
    assert!(matches!(err, SemanticRefactorError::InlineNotSupported));
}

#[test]
fn inline_variable_try_with_resources_declaration_is_not_supported() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  Foo foo() { return null; }

  void m() {
    try (Foo a = foo()) {
      System.out.println(a);
    }
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("Foo a =").unwrap() + "Foo ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap_err();
    assert!(matches!(err, SemanticRefactorError::InlineNotSupported));
}

#[test]
fn inline_variable_rejects_try_with_resources_resource_specification() {
    let file = FileId::new("Test.java");
    let src = r#"class C {
  java.io.InputStream make() { return null; }
  void m() throws Exception {
    java.io.InputStream r = make();
    try (r) {
      r.read();
    }
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("InputStream r =").unwrap() + "InputStream ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at r");

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap_err();
    assert!(matches!(err, SemanticRefactorError::InlineNotSupported));
}

#[test]
fn inline_variable_reassigned_variable_is_not_supported() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int a = 1;
    a = 2;
    System.out.println(a);
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let mut refs = db.find_references(symbol);
    refs.sort_by_key(|r| r.range.start);
    assert_eq!(refs.len(), 2, "expected two references");
    let usage = refs[1].range;

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: false,
            usage_range: Some(usage),
        },
    )
    .unwrap_err();
    assert!(matches!(err, SemanticRefactorError::InlineNotSupported));

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap_err();
    assert!(matches!(err, SemanticRefactorError::InlineNotSupported));
}

#[test]
fn inline_variable_increment_is_rejected() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int a = 1;
    a++;
    System.out.println(a);
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let mut refs = db.find_references(symbol);
    refs.sort_by_key(|r| r.range.start);
    assert_eq!(refs.len(), 2, "expected two references");
    let usage = refs[1].range;

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: false,
            usage_range: Some(usage),
        },
    )
    .unwrap_err();
    assert!(matches!(err, SemanticRefactorError::InlineNotSupported));

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap_err();
    assert!(matches!(err, SemanticRefactorError::InlineNotSupported));
}

#[test]
fn inline_variable_allows_inlining_array_index_on_assignment_lhs() {
    let file = FileId::new("Test.java");
    let src = r#"class C {
  void m(int[] arr) {
    int idx = 0;
    arr[idx] = 1;
    System.out.println(idx);
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int idx").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at idx");

    let edit = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    let expected = r#"class C {
  void m(int[] arr) {
    arr[0] = 1;
    System.out.println(0);
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn inline_variable_allows_inlining_array_index_in_increment_target() {
    let file = FileId::new("Test.java");
    let src = r#"class C {
  void m(int[] arr) {
    int idx = 0;
    arr[idx]++;
    System.out.println(idx);
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int idx").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at idx");

    let edit = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    let expected = r#"class C {
  void m(int[] arr) {
    arr[0]++;
    System.out.println(0);
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn rename_static_imported_field_updates_import_and_usage() {
    let a_file = FileId::new("A.java");
    let b_file = FileId::new("B.java");
    let a_src = r#"package p;

class A {
  static int foo;
  static void bar() {}
}
"#;
    let b_src = r#"package p;

import static p.A.foo;
import static p.A.bar;

class B {
  void m() {
    foo = 1;
    bar();
  }
}
"#;

    let db = RefactorJavaDatabase::new([
        (a_file.clone(), a_src.to_string()),
        (b_file.clone(), b_src.to_string()),
    ]);

    let offset = a_src.find("foo").unwrap() + 1;
    let symbol = db.symbol_at(&a_file, offset).expect("symbol at foo");

    let edit = materialize(
        &db,
        [SemanticChange::Rename {
            symbol,
            new_name: "baz".into(),
        }],
    )
    .unwrap();

    let files = BTreeMap::from([
        (a_file.clone(), a_src.to_string()),
        (b_file.clone(), b_src.to_string()),
    ]);
    let updated = apply_workspace_edit(&files, &edit).unwrap();
    let b_after = updated.get(&b_file).unwrap();

    assert!(
        b_after.contains("import static p.A.baz;"),
        "expected static import to update: {b_after}"
    );
    assert!(
        b_after.contains("baz = 1;"),
        "expected unqualified field usage to update: {b_after}"
    );
    assert!(
        b_after.contains("bar();"),
        "expected other static import to remain unchanged: {b_after}"
    );
}

#[test]
fn rename_static_imported_method_updates_import_and_usage() {
    let a_file = FileId::new("A.java");
    let b_file = FileId::new("B.java");
    let a_src = r#"package p;

class A {
  static int foo;
  static void bar() {}
}
"#;
    let b_src = r#"package p;

import static p.A.foo;
import static p.A.bar;

class B {
  void m() {
    foo = 1;
    bar();
  }
}
"#;

    let db = RefactorJavaDatabase::new([
        (a_file.clone(), a_src.to_string()),
        (b_file.clone(), b_src.to_string()),
    ]);

    let offset = a_src.find("bar").unwrap() + 1;
    let symbol = db.symbol_at(&a_file, offset).expect("symbol at bar");

    let edit = materialize(
        &db,
        [SemanticChange::Rename {
            symbol,
            new_name: "qux".into(),
        }],
    )
    .unwrap();

    let files = BTreeMap::from([
        (a_file.clone(), a_src.to_string()),
        (b_file.clone(), b_src.to_string()),
    ]);
    let updated = apply_workspace_edit(&files, &edit).unwrap();
    let b_after = updated.get(&b_file).unwrap();

    assert!(
        b_after.contains("import static p.A.qux;"),
        "expected static import to update: {b_after}"
    );
    assert!(
        b_after.contains("qux();"),
        "expected unqualified method call to update: {b_after}"
    );
    assert!(
        b_after.contains("foo = 1;"),
        "expected other static import to remain unchanged: {b_after}"
    );
}

#[test]
fn static_import_member_is_not_indexed_when_name_is_ambiguous() {
    // `import static p.A.foo;` imports *all* static members named `foo`.
    // When both a field and a method share the name, the import is not a reference to a single
    // symbol, so semantic rename should not treat it as one.
    let a_file = FileId::new("A.java");
    let b_file = FileId::new("B.java");
    let a_src = r#"package p;

class A {
  static int foo;
  static void foo() {}
}
"#;
    let b_src = r#"package p;

import static p.A.foo;

class B {
  void m() {
    foo = 1;
    foo();
  }
}
"#;

    let db = RefactorJavaDatabase::new([
        (a_file.clone(), a_src.to_string()),
        (b_file.clone(), b_src.to_string()),
    ]);

    let import_start = b_src.find("import static p.A.foo;").unwrap();
    let member_start = import_start + "import static p.A.".len();
    let member_range = WorkspaceTextRange::new(member_start, member_start + "foo".len());

    let field_offset = a_src.find("static int foo").unwrap() + "static int ".len() + 1;
    let field_symbol = db
        .symbol_at(&a_file, field_offset)
        .expect("symbol at field foo");
    assert_eq!(db.symbol_kind(field_symbol), Some(JavaSymbolKind::Field));

    let method_offset = a_src.find("static void foo").unwrap() + "static void ".len() + 1;
    let method_symbol = db
        .symbol_at(&a_file, method_offset)
        .expect("symbol at method foo");
    assert_eq!(db.symbol_kind(method_symbol), Some(JavaSymbolKind::Method));

    let field_refs = db.find_references(field_symbol);
    assert!(
        !field_refs
            .iter()
            .any(|r| r.file == b_file && r.range == member_range),
        "did not expect static import to be indexed as a field reference: {field_refs:?}"
    );

    let method_refs = db.find_references(method_symbol);
    assert!(
        !method_refs
            .iter()
            .any(|r| r.file == b_file && r.range == member_range),
        "did not expect static import to be indexed as a method reference: {method_refs:?}"
    );
}

#[test]
fn inline_variable_rejects_crossing_lambda_execution_context() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
      void m() {
    int x = 1;
    int a = x;
    Runnable r = () -> System.out.println(a);
    x = 2;
    r.run();
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let mut refs = db.find_references(symbol);
    assert_eq!(refs.len(), 1, "expected a single usage of a");
    let usage = refs.pop().unwrap().range;

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: false,
            usage_range: Some(usage),
        },
    )
    .unwrap_err();
    assert!(matches!(err, SemanticRefactorError::InlineNotSupported));
}

#[test]
fn inline_variable_rejects_crossing_anonymous_class_boundary() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int x = 1;
    int a = x;
    Runnable r = new Runnable() {
      public void run() { System.out.println(a); }
    };
    x = 2;
    r.run();
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap_err();
    assert!(matches!(err, SemanticRefactorError::InlineNotSupported));
}

#[test]
fn inline_variable_rejects_mutated_dependency() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int x = 1;
    int a = x;
    x = 2;
    System.out.println(a);
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let mut refs = db.find_references(symbol);
    assert_eq!(refs.len(), 1, "expected a single usage of a");
    let usage = refs.pop().unwrap().range;

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: false,
            usage_range: Some(usage),
        },
    )
    .unwrap_err();
    assert!(matches!(
        err,
        SemanticRefactorError::InlineWouldChangeValue { .. }
    ));
}

#[test]
fn inline_variable_allows_stable_dependency() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m(int x) {
    int a = x;
    System.out.println(a);
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let edit = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap();
    let after = apply_text_edits(src, &edit.text_edits).unwrap();

    let expected = r#"class Test {
  void m(int x) {
    System.out.println(x);
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn inline_variable_allows_field_dependency_when_no_writes_or_shadowing() {
    let file = FileId::new("Test.java");
    let src = r#"class C { int x = 1; void m() { int a = x; System.out.println(a); } }
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let edit = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    let expected = r#"class C { int x = 1; void m() { System.out.println(x); } }
"#;
    assert_eq!(after, expected);
}

#[test]
fn inline_variable_allows_inlining_within_same_anonymous_class_body() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    Runnable r = new Runnable() {
      public void run() {
        int a = 1 + 2;
        System.out.println(a);
      }
    };
  }
}
"#;

    // `RefactorJavaDatabase` does not currently index local declarations inside anonymous class
    // bodies. Inline Variable itself is syntax-driven once we have semantic inputs, so test it with
    // a small stub `RefactorDatabase`.
    let dummy_file = FileId::new("Dummy.java");
    let dummy_src = "class Dummy { void m() { int x = 0; } }";
    let dummy_db = RefactorJavaDatabase::new([(dummy_file.clone(), dummy_src.to_string())]);
    let dummy_offset = dummy_src.find("int x").unwrap() + "int ".len();
    let symbol = dummy_db
        .symbol_at(&dummy_file, dummy_offset)
        .expect("dummy symbol");

    let decl_offset = src.find("int a").unwrap() + "int ".len();
    let name_range = WorkspaceTextRange::new(decl_offset, decl_offset + 1);

    let usage_offset = src.find("println(a)").unwrap() + "println(".len();
    let usage_range = WorkspaceTextRange::new(usage_offset, usage_offset + 1);

    let def = SymbolDefinition {
        file: file.clone(),
        name: "a".to_string(),
        name_range,
        scope: 0,
    };
    let refs = vec![Reference {
        file: file.clone(),
        range: usage_range,
        scope: Some(def.scope),
        kind: ReferenceKind::Name,
    }];

    struct SingleSymbolDb {
        file: FileId,
        text: String,
        symbol: SymbolId,
        def: SymbolDefinition,
        refs: Vec<Reference>,
    }

    impl RefactorDatabase for SingleSymbolDb {
        fn file_text(&self, file: &FileId) -> Option<&str> {
            (file == &self.file).then_some(&self.text)
        }

        fn symbol_at(&self, file: &FileId, offset: usize) -> Option<SymbolId> {
            if file != &self.file {
                return None;
            }
            let in_def = self.def.name_range.start <= offset && offset < self.def.name_range.end;
            let in_ref = self
                .refs
                .iter()
                .any(|r| r.range.start <= offset && offset < r.range.end);
            (in_def || in_ref).then_some(self.symbol)
        }

        fn symbol_definition(&self, symbol: SymbolId) -> Option<SymbolDefinition> {
            (symbol == self.symbol).then(|| self.def.clone())
        }

        fn symbol_scope(&self, symbol: SymbolId) -> Option<u32> {
            (symbol == self.symbol).then_some(self.def.scope)
        }

        fn symbol_kind(&self, symbol: SymbolId) -> Option<JavaSymbolKind> {
            (symbol == self.symbol).then_some(JavaSymbolKind::Local)
        }

        fn resolve_name_in_scope(&self, scope: u32, name: &str) -> Option<SymbolId> {
            (scope == self.def.scope && name == self.def.name).then_some(self.symbol)
        }

        fn would_shadow(&self, _scope: u32, _name: &str) -> Option<SymbolId> {
            None
        }

        fn find_references(&self, symbol: SymbolId) -> Vec<Reference> {
            if symbol == self.symbol {
                self.refs.clone()
            } else {
                Vec::new()
            }
        }

        fn resolve_name_expr(&self, file: &FileId, range: WorkspaceTextRange) -> Option<SymbolId> {
            if file != &self.file {
                return None;
            }
            self.refs
                .iter()
                .any(|r| r.range == range)
                .then_some(self.symbol)
        }
    }

    let db = SingleSymbolDb {
        file: file.clone(),
        text: src.to_string(),
        symbol,
        def,
        refs,
    };

    let edit = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap();
    let after = apply_text_edits(src, &edit.text_edits).unwrap();

    let expected = r#"class Test {
  void m() {
    Runnable r = new Runnable() {
      public void run() {
        System.out.println((1 + 2));
      }
    };
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn inline_variable_allows_inlining_within_same_lambda_execution_context() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    Runnable r = () -> {
      int a = 1 + 2;
      System.out.println(a);
    };
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let edit = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    let expected = r#"class Test {
  void m() {
    Runnable r = () -> {
      System.out.println((1 + 2));
    };
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn inline_variable_rejects_lambda_capture_breakage() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int x = 0;
    int a = x;
    x++;
    Runnable r = () -> System.out.println(a);
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let mut refs = db.find_references(symbol);
    assert_eq!(refs.len(), 1, "expected a single usage of a");
    let usage = refs.pop().unwrap().range;

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: false,
            usage_range: Some(usage),
        },
    )
    .unwrap_err();
    assert!(matches!(err, SemanticRefactorError::InlineNotSupported));
}

#[test]
fn inline_variable_array_creation_initializer_is_side_effectful() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int[] a = new int[1];
    System.out.println(a);
    System.out.println(a);
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int[] a").unwrap() + "int[] ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap_err();
    assert!(matches!(err, SemanticRefactorError::InlineSideEffects));
}

#[test]
fn rename_local_variable_inside_array_access() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m(int[] arr) {
    int foo = 1;
    System.out.println(arr[foo]);
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int foo").unwrap() + "int ".len() + 1;
    let symbol = db.symbol_at(&file, offset).expect("symbol at foo");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "bar".into(),
        },
    )
    .unwrap();
    let after = apply_text_edits(src, &edit.text_edits).unwrap();

    assert!(
        after.contains("int bar = 1;"),
        "expected foo declaration to be renamed: {after}"
    );
    assert!(
        after.contains("arr[bar]"),
        "expected foo usage inside array access to be renamed: {after}"
    );
    assert!(
        !after.contains("foo"),
        "expected all occurrences of foo to be renamed: {after}"
    );
}

#[test]
fn rename_local_variable_inside_array_creation_expression() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int foo = 1;
    int[] arr = new int[foo];
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int foo").unwrap() + "int ".len() + 1;
    let symbol = db.symbol_at(&file, offset).expect("symbol at foo");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "bar".into(),
        },
    )
    .unwrap();
    let after = apply_text_edits(src, &edit.text_edits).unwrap();

    assert!(
        after.contains("int bar = 1;"),
        "expected foo declaration to be renamed: {after}"
    );
    assert!(
        after.contains("new int[bar]"),
        "expected foo usage inside array creation to be renamed: {after}"
    );
    assert!(
        !after.contains("new int[foo]"),
        "expected old name to be gone: {after}"
    );
}

#[test]
fn rename_outer_local_does_not_touch_names_inside_switch_expression_yield_block() {
    // The Java switch expression body may contain blocks and declarations that introduce bindings
    // we don't currently model in the lightweight AST / stable HIR. We must not accidentally treat
    // references inside those blocks as referring to the outer scope.
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  int m(int x) {
    int foo = 0;
    return switch (x) {
      default -> {
        int foo = 1;
        yield foo;
      }
    };
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int foo = 0").unwrap() + "int ".len() + 1;
    let symbol = db.symbol_at(&file, offset).expect("symbol at outer foo");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "bar".into(),
        },
    )
    .unwrap();
    let after = apply_text_edits(src, &edit.text_edits).unwrap();

    // Outer binding renamed.
    assert!(
        after.contains("int bar = 0;"),
        "expected outer foo declaration to be renamed: {after}"
    );
    // Inner binding untouched.
    assert!(
        after.contains("int foo = 1;"),
        "expected inner foo declaration to remain unchanged: {after}"
    );
    assert!(
        after.contains("yield foo;"),
        "expected inner foo usage to remain unchanged: {after}"
    );
    assert!(
        !after.contains("yield bar;"),
        "expected outer rename not to affect inner yield usage: {after}"
    );
}

#[test]
fn rename_local_variable_inside_instanceof_expression() {
    let file = FileId::new("Test.java");
    let src = r#"class Foo {}

class Test {
  void m(Object x) {
    boolean b = (x instanceof Foo);
    System.out.println(b);
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("Object x").unwrap() + "Object ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at x");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "y".into(),
        },
    )
    .unwrap();
    let after = apply_text_edits(src, &edit.text_edits).unwrap();

    assert!(
        after.contains("void m(Object y)"),
        "expected parameter x to be renamed: {after}"
    );
    assert!(
        after.contains("(y instanceof Foo)"),
        "expected x usage inside instanceof to be renamed: {after}"
    );
    assert!(
        !after.contains("(x instanceof Foo)"),
        "expected old name to be gone: {after}"
    );
}

#[test]
fn rename_local_variable_inside_array_access_nested_under_field_access() {
    let file = FileId::new("Test.java");
    let src = r#"class Foo { int foo = 0; }

class Test {
  void m(Foo[] arr) {
    int i = 0;
    System.out.println(arr[i].foo);
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int i").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at i");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "j".into(),
        },
    )
    .unwrap();
    let after = apply_text_edits(src, &edit.text_edits).unwrap();

    assert!(
        after.contains("int j = 0;"),
        "expected i declaration to be renamed: {after}"
    );
    assert!(
        after.contains("arr[j].foo"),
        "expected i usage inside array access receiver to be renamed: {after}"
    );
    assert!(
        !after.contains("arr[i].foo"),
        "expected old name to be gone: {after}"
    );
}

#[test]
fn extract_variable_rejects_assert_condition_extraction() {
    let fixture = r#"class C { void m(int x) { assert /*start*/x > 0/*end*/; } }"#;
    let (src, selection) = extract_range(fixture);
    let file = FileId::new("Test.java");
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file,
            expr_range: selection,
            name: "cond".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(err, SemanticRefactorError::ExtractNotSupportedInAssert),
        "expected ExtractNotSupportedInAssert for assert condition, got: {err:?}"
    );
}

#[test]
fn extract_variable_rejects_assert_message_extraction() {
    let fixture = r#"class C { void m(int x) { assert x > 0 : /*start*/x/*end*/; } }"#;
    let (src, selection) = extract_range(fixture);
    let file = FileId::new("Test.java");
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let err = extract_variable(
        &db,
        ExtractVariableParams {
            file,
            expr_range: selection,
            name: "msg".into(),
            use_var: true,
            replace_all: false,
        },
    )
    .unwrap_err();

    assert!(
        matches!(err, SemanticRefactorError::ExtractNotSupportedInAssert),
        "expected ExtractNotSupportedInAssert for assert message, got: {err:?}"
    );
}

#[test]
fn rename_nested_type_updates_qualified_expression_reference() {
    let file = FileId::new("Test.java");
    let src = r#"class Outer {
  static class Inner {
    static void m() {}
  }
}

class Use {
  void f() {
    Outer.Inner.m();
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("Outer.Inner.m").unwrap() + "Outer.".len() + 1;
    let symbol = db.symbol_at(&file, offset).expect("symbol at Inner");
    assert_eq!(db.symbol_kind(symbol), Some(JavaSymbolKind::Type));

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "Renamed".into(),
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    assert!(after.contains("class Renamed"));
    assert!(after.contains("Outer.Renamed.m()"));
    assert!(!after.contains("Outer.Inner.m()"));
}

#[test]
fn rename_nested_type_updates_qualified_method_reference_receiver() {
    let file = FileId::new("Test.java");
    let src = r#"class Outer {
  static class Inner {
    static void m() {}
  }
}

class Use {
  void f() {
    Runnable r = Outer.Inner::m;
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("Outer.Inner::m").unwrap() + "Outer.".len() + 1;
    let symbol = db.symbol_at(&file, offset).expect("symbol at Inner");
    assert_eq!(db.symbol_kind(symbol), Some(JavaSymbolKind::Type));

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "Renamed".into(),
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    assert!(after.contains("class Renamed"));
    assert!(after.contains("Outer.Renamed::m"));
    assert!(!after.contains("Outer.Inner::m"));
}

#[test]
fn rename_static_method_called_via_nested_type_updates_call_site() {
    let file = FileId::new("Test.java");
    let src = r#"class Outer {
  static class Inner {
    static void m() {}
  }
}

class Use {
  void f() {
    Outer.Inner.m();
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("Outer.Inner.m").unwrap() + "Outer.Inner.".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at m");
    assert_eq!(db.symbol_kind(symbol), Some(JavaSymbolKind::Method));

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "n".into(),
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    assert!(after.contains("static void n()"));
    assert!(after.contains("Outer.Inner.n()"));
    assert!(!after.contains("Outer.Inner.m()"));
}

#[test]
fn rename_static_method_referenced_via_nested_type_updates_site() {
    let file = FileId::new("Test.java");
    let src = r#"class Outer {
  static class Inner {
    static void m() {}
  }
}

class Use {
  void f() {
    Runnable r = Outer.Inner::m;
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("Outer.Inner::m").unwrap() + "Outer.Inner::".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at m");
    assert_eq!(db.symbol_kind(symbol), Some(JavaSymbolKind::Method));

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "n".into(),
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    assert!(after.contains("static void n()"));
    assert!(after.contains("Outer.Inner::n"));
    assert!(!after.contains("Outer.Inner::m"));
}

#[test]
fn rename_field_updates_qualified_outer_this_reference() {
    let file = FileId::new("Test.java");
    let src = r#"class Outer {
  int foo = 0;

  class Inner {
    int foo = 1;

    int m() {
      return foo + Outer.this.foo;
    }
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("int foo = 0").unwrap() + "int ".len() + 1;
    let symbol = db
        .symbol_at(&file, offset)
        .expect("symbol at outer field foo");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "bar".into(),
        },
    )
    .unwrap();
    let after = apply_text_edits(src, &edit.text_edits).unwrap();

    assert!(
        after.contains("int bar = 0;"),
        "outer field declaration should be renamed: {after}"
    );
    assert!(
        after.contains("int foo = 1;"),
        "inner field declaration should remain unchanged: {after}"
    );
    assert!(
        after.contains("return foo + Outer.this.bar;"),
        "qualified outer field reference should be renamed: {after}"
    );
    assert!(
        !after.contains("Outer.this.foo"),
        "old qualified reference should not remain: {after}"
    );
}

#[test]
fn rename_field_updates_parenthesized_qualified_outer_this_reference() {
    let file = FileId::new("Test.java");
    let src = r#"class Outer {
  int foo = 0;

  class Inner {
    int foo = 1;

    int m() {
      return foo + (Outer.this).foo;
    }
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("int foo = 0").unwrap() + "int ".len() + 1;
    let symbol = db
        .symbol_at(&file, offset)
        .expect("symbol at outer field foo");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "bar".into(),
        },
    )
    .unwrap();
    let after = apply_text_edits(src, &edit.text_edits).unwrap();

    assert!(after.contains("int bar = 0;"));
    assert!(after.contains("int foo = 1;"));
    assert!(after.contains("return foo + (Outer.this).bar;"));
    assert!(!after.contains("(Outer.this).foo"));
}

#[test]
fn rename_method_updates_qualified_outer_this_call() {
    let file = FileId::new("Test.java");
    let src = r#"class Outer {
  void m() {}

  class Inner {
    void m() {}

    void call() {
      Outer.this.m();
    }
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("void m()").unwrap() + "void ".len();
    let symbol = db
        .symbol_at(&file, offset)
        .expect("symbol at outer method m");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "n".into(),
        },
    )
    .unwrap();
    let after = apply_text_edits(src, &edit.text_edits).unwrap();

    assert!(
        after.contains("void n()"),
        "outer method declaration should be renamed: {after}"
    );
    assert!(
        after.contains("void m() {}"),
        "inner method declaration should remain unchanged: {after}"
    );
    assert!(
        after.contains("Outer.this.n();"),
        "qualified outer method call should be renamed: {after}"
    );
    assert!(
        !after.contains("Outer.this.m();"),
        "old qualified call should not remain: {after}"
    );
}

#[test]
fn rename_method_updates_qualified_outer_this_method_reference() {
    let file = FileId::new("Test.java");
    let src = r#"class Outer {
  void m() {}

  class Inner {
    void f() {
      Runnable r = Outer.this::m;
    }
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("void m()").unwrap() + "void ".len();
    let symbol = db
        .symbol_at(&file, offset)
        .expect("symbol at outer method m");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "n".into(),
        },
    )
    .unwrap();
    let after = apply_text_edits(src, &edit.text_edits).unwrap();

    assert!(
        after.contains("void n()"),
        "outer method declaration should be renamed: {after}"
    );
    assert!(
        after.contains("Outer.this::n"),
        "qualified outer method reference should be renamed: {after}"
    );
    assert!(
        !after.contains("Outer.this::m"),
        "old qualified reference should not remain: {after}"
    );
}

#[test]
fn rename_method_updates_parenthesized_qualified_outer_this_method_reference() {
    let file = FileId::new("Test.java");
    let src = r#"class Outer {
  void m() {}

  class Inner {
    void f() {
      Runnable r = (Outer.this)::m;
    }
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("void m()").unwrap() + "void ".len();
    let symbol = db
        .symbol_at(&file, offset)
        .expect("symbol at outer method m");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "n".into(),
        },
    )
    .unwrap();
    let after = apply_text_edits(src, &edit.text_edits).unwrap();

    assert!(after.contains("void n()"));
    assert!(after.contains("(Outer.this)::n"));
    assert!(!after.contains("(Outer.this)::m"));
}

#[test]
fn rename_method_updates_qualified_outer_super_call() {
    let file = FileId::new("Test.java");
    let src = r#"class Base {
  void foo() {}
}

class Outer extends Base {
  class Inner {
    void call() {
      Outer.super.foo();
    }
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("void foo()").unwrap() + "void ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at Base.foo");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "bar".into(),
        },
    )
    .unwrap();
    let after = apply_text_edits(src, &edit.text_edits).unwrap();

    assert!(after.contains("void bar()"));
    assert!(after.contains("Outer.super.bar();"));
    assert!(!after.contains("Outer.super.foo();"));
}

#[test]
fn rename_fully_qualified_type_in_expression_updates_segment() {
    let foo_file = FileId::new("Foo.java");
    let use_file = FileId::new("Use.java");

    let foo_src = r#"package com.example;

class Foo {
  static void staticM() {}
}
"#;

    let use_src = r#"class Use {
  void f() {
    com.example.Foo.staticM();
  }
}
"#;

    let db = RefactorJavaDatabase::new([
        (foo_file.clone(), foo_src.to_string()),
        (use_file.clone(), use_src.to_string()),
    ]);

    let offset = use_src.find("com.example.Foo").unwrap() + "com.example.".len() + 1;
    let symbol = db.symbol_at(&use_file, offset).expect("symbol at Foo");
    assert_eq!(db.symbol_kind(symbol), Some(JavaSymbolKind::Type));

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "Bar".into(),
        },
    )
    .unwrap();

    let foo_edits: Vec<_> = edit
        .text_edits
        .iter()
        .filter(|e| e.file == foo_file)
        .cloned()
        .collect();
    let use_edits: Vec<_> = edit
        .text_edits
        .iter()
        .filter(|e| e.file == use_file)
        .cloned()
        .collect();

    let updated_foo = apply_text_edits(foo_src, &foo_edits).unwrap();
    let updated_use = apply_text_edits(use_src, &use_edits).unwrap();

    assert!(updated_use.contains("com.example.Bar.staticM()"));
    assert!(!updated_use.contains("com.example.Foo.staticM()"));

    assert!(updated_foo.contains("class Bar"));
    assert!(!updated_foo.contains("class Foo"));
}

#[test]
fn rename_fully_qualified_type_in_method_reference_updates_segment() {
    let foo_file = FileId::new("Foo.java");
    let use_file = FileId::new("Use.java");

    let foo_src = r#"package com.example;

class Foo {
  static void staticM() {}
}
"#;

    let use_src = r#"class Use {
  void f() {
    Runnable r = com.example.Foo::staticM;
  }
}
"#;

    let db = RefactorJavaDatabase::new([
        (foo_file.clone(), foo_src.to_string()),
        (use_file.clone(), use_src.to_string()),
    ]);

    let offset = use_src.find("com.example.Foo::staticM").unwrap() + "com.example.".len() + 1;
    let symbol = db.symbol_at(&use_file, offset).expect("symbol at Foo");
    assert_eq!(db.symbol_kind(symbol), Some(JavaSymbolKind::Type));

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "Bar".into(),
        },
    )
    .unwrap();

    let foo_edits: Vec<_> = edit
        .text_edits
        .iter()
        .filter(|e| e.file == foo_file)
        .cloned()
        .collect();
    let use_edits: Vec<_> = edit
        .text_edits
        .iter()
        .filter(|e| e.file == use_file)
        .cloned()
        .collect();

    let updated_foo = apply_text_edits(foo_src, &foo_edits).unwrap();
    let updated_use = apply_text_edits(use_src, &use_edits).unwrap();

    assert!(updated_use.contains("com.example.Bar::staticM"));
    assert!(!updated_use.contains("com.example.Foo::staticM"));

    assert!(updated_foo.contains("class Bar"));
    assert!(!updated_foo.contains("class Foo"));
}

#[test]
fn rename_static_method_called_via_fully_qualified_type_updates_call_site() {
    let foo_file = FileId::new("Foo.java");
    let use_file = FileId::new("Use.java");

    let foo_src = r#"package com.example;

class Foo {
  static void staticM() {}
}
"#;

    let use_src = r#"class Use {
  void f() {
    com.example.Foo.staticM();
  }
}
"#;

    let db = RefactorJavaDatabase::new([
        (foo_file.clone(), foo_src.to_string()),
        (use_file.clone(), use_src.to_string()),
    ]);

    let offset = use_src.find("com.example.Foo.staticM").unwrap() + "com.example.Foo.".len();
    let symbol = db.symbol_at(&use_file, offset).expect("symbol at staticM");
    assert_eq!(db.symbol_kind(symbol), Some(JavaSymbolKind::Method));

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "renamed".into(),
        },
    )
    .unwrap();

    let foo_edits: Vec<_> = edit
        .text_edits
        .iter()
        .filter(|e| e.file == foo_file)
        .cloned()
        .collect();
    let use_edits: Vec<_> = edit
        .text_edits
        .iter()
        .filter(|e| e.file == use_file)
        .cloned()
        .collect();

    let updated_foo = apply_text_edits(foo_src, &foo_edits).unwrap();
    let updated_use = apply_text_edits(use_src, &use_edits).unwrap();

    assert!(updated_use.contains("com.example.Foo.renamed()"));
    assert!(!updated_use.contains("com.example.Foo.staticM()"));

    assert!(updated_foo.contains("static void renamed()"));
    assert!(!updated_foo.contains("static void staticM()"));
}

#[test]
fn rename_static_method_referenced_via_fully_qualified_type_updates_site() {
    let foo_file = FileId::new("Foo.java");
    let use_file = FileId::new("Use.java");

    let foo_src = r#"package com.example;

class Foo {
  static void staticM() {}
}
"#;

    let use_src = r#"class Use {
  void f() {
    Runnable r = com.example.Foo::staticM;
  }
}
"#;

    let db = RefactorJavaDatabase::new([
        (foo_file.clone(), foo_src.to_string()),
        (use_file.clone(), use_src.to_string()),
    ]);

    let offset = use_src.find("com.example.Foo::staticM").unwrap() + "com.example.Foo::".len() + 1;
    let symbol = db.symbol_at(&use_file, offset).expect("symbol at staticM");
    assert_eq!(db.symbol_kind(symbol), Some(JavaSymbolKind::Method));

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "renamed".into(),
        },
    )
    .unwrap();

    let foo_edits: Vec<_> = edit
        .text_edits
        .iter()
        .filter(|e| e.file == foo_file)
        .cloned()
        .collect();
    let use_edits: Vec<_> = edit
        .text_edits
        .iter()
        .filter(|e| e.file == use_file)
        .cloned()
        .collect();

    let updated_foo = apply_text_edits(foo_src, &foo_edits).unwrap();
    let updated_use = apply_text_edits(use_src, &use_edits).unwrap();

    assert!(updated_use.contains("com.example.Foo::renamed"));
    assert!(!updated_use.contains("com.example.Foo::staticM"));

    assert!(updated_foo.contains("static void renamed()"));
    assert!(!updated_foo.contains("static void staticM()"));
}

#[test]
fn rename_lambda_parameter_expression_body_updates_all_occurrences() {
    let file = FileId::new("Test.java");
    let src = r#"class C { void m(){ java.util.function.Function<Integer,String> f = x -> x.toString(); } }"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let decl_offset = src.find("x ->").unwrap();
    let usage_offset = src.find("-> x.toString").unwrap() + "-> ".len();

    let decl_symbol = db
        .symbol_at(&file, decl_offset)
        .expect("symbol at lambda parameter x");
    let usage_symbol = db
        .symbol_at(&file, usage_offset)
        .expect("symbol at lambda parameter usage x");
    assert_eq!(decl_symbol, usage_symbol);

    let edit = rename(
        &db,
        RenameParams {
            symbol: decl_symbol,
            new_name: "y".into(),
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    assert!(
        after.contains("f = y -> y.toString()"),
        "expected both occurrences to be renamed: {after}"
    );
    assert!(
        !after.contains("x ->") && !after.contains("-> x.toString"),
        "expected no remaining x references: {after}"
    );
}

#[test]
fn rename_lambda_parameter_block_body_updates_all_occurrences() {
    let file = FileId::new("Test.java");
    let src = r#"class C { void m(){ java.util.function.Function<Integer,Integer> f = (x) -> { return x + 1; }; } }"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let decl_offset = src.find("(x)").unwrap() + 1; // inside parens
    let usage_offset = src.find("return x").unwrap() + "return ".len();

    let decl_symbol = db
        .symbol_at(&file, decl_offset)
        .expect("symbol at lambda parameter x");
    let usage_symbol = db
        .symbol_at(&file, usage_offset)
        .expect("symbol at lambda parameter usage x");
    assert_eq!(decl_symbol, usage_symbol);

    let edit = rename(
        &db,
        RenameParams {
            symbol: decl_symbol,
            new_name: "y".into(),
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    assert!(
        after.contains("f = (y) -> { return y + 1; }"),
        "expected both occurrences to be renamed: {after}"
    );
    assert!(
        !after.contains("(x)") && !after.contains("return x"),
        "expected no remaining x references: {after}"
    );
}

#[test]
fn rename_lambda_parameter_multi_param_updates_all_occurrences() {
    let file = FileId::new("Test.java");
    let src = r#"class C { void m(){ java.util.function.BiFunction<Integer,Integer,Integer> f = (x, y) -> x + y; } }"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let decl_offset = src.find("(x").unwrap() + 1;
    let usage_offset = src.find("-> x +").unwrap() + "-> ".len();

    let decl_symbol = db
        .symbol_at(&file, decl_offset)
        .expect("symbol at lambda parameter x");
    let usage_symbol = db
        .symbol_at(&file, usage_offset)
        .expect("symbol at lambda parameter usage x");
    assert_eq!(decl_symbol, usage_symbol);

    let edit = rename(
        &db,
        RenameParams {
            symbol: decl_symbol,
            new_name: "z".into(),
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    assert!(
        after.contains("f = (z, y) -> z + y"),
        "expected both occurrences to be renamed: {after}"
    );
    assert!(
        !after.contains("(x, y)") && !after.contains("-> x +"),
        "expected no remaining x references: {after}"
    );
}

#[test]
fn rename_lambda_parameter_typed_param_updates_all_occurrences() {
    let file = FileId::new("Test.java");
    let src =
        r#"class C { void m(){ java.util.function.IntUnaryOperator f = (int x) -> x + 1; } }"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let decl_offset = src.find("int x").unwrap() + "int ".len();
    let usage_offset = src.find("-> x + 1").unwrap() + "-> ".len();

    let decl_symbol = db
        .symbol_at(&file, decl_offset)
        .expect("symbol at lambda parameter x");
    let usage_symbol = db
        .symbol_at(&file, usage_offset)
        .expect("symbol at lambda parameter usage x");
    assert_eq!(decl_symbol, usage_symbol);

    let edit = rename(
        &db,
        RenameParams {
            symbol: decl_symbol,
            new_name: "y".into(),
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    assert!(
        after.contains("f = (int y) -> y + 1"),
        "expected both occurrences to be renamed: {after}"
    );
    assert!(
        !after.contains("int x") && !after.contains("-> x + 1"),
        "expected no remaining x references: {after}"
    );
}

#[test]
fn rename_lambda_parameter_conflict_with_local_in_body_is_rejected() {
    let file = FileId::new("Test.java");
    let src = r#"class C { void m(){ java.util.function.IntUnaryOperator f = (x) -> { int y = 1; return x + y; }; } }"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("(x)").unwrap() + 1;
    let symbol = db
        .symbol_at(&file, offset)
        .expect("symbol at lambda parameter x");

    let err = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "y".into(),
        },
    )
    .unwrap_err();
    let SemanticRefactorError::Conflicts(conflicts) = err else {
        panic!("expected conflicts, got: {err:?}");
    };

    assert!(
        conflicts
            .iter()
            .any(|c| matches!(c, Conflict::NameCollision { name, .. } if name == "y")),
        "expected NameCollision conflict: {conflicts:?}"
    );
}

#[test]
fn rename_lambda_parameter_conflict_with_local_in_nested_block_is_rejected() {
    let file = FileId::new("Test.java");
    let src = r#"class C { void m(){ java.util.function.IntUnaryOperator f = (x) -> { { int y = 1; } return x + 1; }; } }"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("(x)").unwrap() + 1;
    let symbol = db
        .symbol_at(&file, offset)
        .expect("symbol at lambda parameter x");

    let err = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "y".into(),
        },
    )
    .unwrap_err();
    let SemanticRefactorError::Conflicts(conflicts) = err else {
        panic!("expected conflicts, got: {err:?}");
    };

    assert!(
        conflicts
            .iter()
            .any(|c| matches!(c, Conflict::NameCollision { name, .. } if name == "y")),
        "expected NameCollision conflict: {conflicts:?}"
    );
}

#[test]
fn inline_variable_rejects_shadowed_dependency_in_nested_block() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int b = 1;
    int a = b;
    {
      int b = 2;
      System.out.println(a);
    }
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap_err();

    assert!(
        matches!(
            &err,
            SemanticRefactorError::InlineShadowedDependency { name } if name == "b"
        ),
        "expected InlineShadowedDependency for `b`, got: {err:?}"
    );
}

#[test]
fn inline_variable_rejects_shadowed_field_dependency_in_nested_block() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  int x = 1;
  void m() {
    int a = x;
    {
      int x = 2;
      System.out.println(a);
    }
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap_err();

    assert!(
        matches!(
            &err,
            SemanticRefactorError::InlineShadowedDependency { name } if name == "x"
        ),
        "expected InlineShadowedDependency for `x`, got: {err:?}"
    );
}

#[test]
fn inline_variable_allows_when_no_shadowing() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int b = 1;
    int a = b;
    System.out.println(a);
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let edit = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    assert!(
        after.contains("println(b);"),
        "expected `a` to be inlined to `b`, got: {after}"
    );
    assert!(
        !after.contains("int a ="),
        "expected `a` declaration to be removed, got: {after}"
    );
}

#[test]
fn inline_variable_rejects_shadowing_by_lambda_parameter() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int b = 1;
    int a = b;
    java.util.function.IntConsumer c = (b) -> System.out.println(a);
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap_err();

    assert!(
        matches!(
            &err,
            SemanticRefactorError::InlineShadowedDependency { name } if name == "b"
        ),
        "expected InlineShadowedDependency for `b`, got: {err:?}"
    );
}

#[test]
fn inline_variable_rejects_shadowing_by_for_header_declaration() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int b = 1;
    int a = b;
    for (int b = 2; b < 3; b++) {
      System.out.println(a);
    }
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap_err();

    assert!(
        matches!(
            &err,
            SemanticRefactorError::InlineShadowedDependency { name } if name == "b"
        ),
        "expected InlineShadowedDependency for `b`, got: {err:?}"
    );
}

#[test]
fn inline_variable_use_in_array_index_is_supported() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int a = 1;
    int[] arr = new int[2];
    arr[a] = 0;
    System.out.println(a);
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let edit = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    let expected = r#"class Test {
  void m() {
    int[] arr = new int[2];
    arr[1] = 0;
    System.out.println(1);
  }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn inline_variable_rejects_shadowing_by_catch_parameter() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int b = 1;
    int a = b;
    try {
      System.out.println("x");
    } catch (Exception b) {
      System.out.println(a);
    }
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap_err();

    assert!(
        matches!(
            &err,
            SemanticRefactorError::InlineShadowedDependency { name } if name == "b"
        ),
        "expected InlineShadowedDependency for `b`, got: {err:?}"
    );
}

#[test]
fn inline_variable_rejects_shadowing_by_try_resource() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int b = 1;
    int a = b;
    try (java.io.ByteArrayInputStream b = new java.io.ByteArrayInputStream(new byte[0])) {
      System.out.println(a);
    }
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let err = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap_err();

    assert!(
        matches!(
            &err,
            SemanticRefactorError::InlineShadowedDependency { name } if name == "b"
        ),
        "expected InlineShadowedDependency for `b`, got: {err:?}"
    );
}

#[test]
fn inline_variable_allows_resource_shadowing_in_catch_clause() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int b = 1;
    int a = b;
    try (java.io.ByteArrayInputStream b = new java.io.ByteArrayInputStream(new byte[0])) {
      System.out.println("x");
    } catch (RuntimeException e) {
      System.out.println(a);
    }
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int a").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at a");

    let edit = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    assert!(
        after.contains("catch (RuntimeException e) {\n      System.out.println(b);"),
        "expected `a` to be inlined to outer `b` in catch clause, got: {after}"
    );
    assert!(
        !after.contains("int a ="),
        "expected `a` declaration to be removed, got: {after}"
    );
}

#[test]
fn symbol_at_returns_type_for_class_name() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  int foo = 0;
  void method() {}
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("class Test").unwrap() + "class ".len() + 1;
    let symbol = db.symbol_at(&file, offset).expect("symbol at type name");
    assert_eq!(db.symbol_kind(symbol), Some(JavaSymbolKind::Type));
}

#[test]
fn symbol_at_returns_field_for_field_identifier() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  int foo = 0;
  void method() {}
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int foo").unwrap() + "int ".len() + 1;
    let symbol = db.symbol_at(&file, offset).expect("symbol at field name");
    assert_eq!(db.symbol_kind(symbol), Some(JavaSymbolKind::Field));
}

#[test]
fn symbol_at_returns_method_for_method_identifier() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  int foo = 0;
  void method() {}
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("void method").unwrap() + "void ".len() + 1;
    let symbol = db.symbol_at(&file, offset).expect("symbol at method name");
    assert_eq!(db.symbol_kind(symbol), Some(JavaSymbolKind::Method));
}

#[test]
fn symbol_at_returns_type_for_nested_class_name() {
    let file = FileId::new("Test.java");
    let src = r#"class Outer {
  class Inner {}
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("class Inner").unwrap() + "class ".len() + 1;
    let symbol = db
        .symbol_at(&file, offset)
        .expect("symbol at nested type name");
    assert_eq!(db.symbol_kind(symbol), Some(JavaSymbolKind::Type));
}

#[test]
fn rename_field_renames_java_bean_accessors_and_call_sites() {
    let file = FileId::new("Test.java");
    let src = r#"class Person {
  private String name;

  public String getName() { return name; }
  public void setName(String name) { this.name = name; }

  void m() {
    Person p = new Person();
    p.getName();
    p.setName("x");
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("String name").unwrap() + "String ".len() + 1;
    let symbol = db.symbol_at(&file, offset).expect("symbol at field name");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "title".into(),
        },
    )
    .unwrap();
    let after = apply_text_edits(src, &edit.text_edits).unwrap();

    assert!(
        after.contains("private String title;"),
        "expected field to be renamed: {after}"
    );
    assert!(
        after.contains("getTitle()"),
        "expected getter to be renamed: {after}"
    );
    assert!(
        after.contains("setTitle(String name)"),
        "expected setter to be renamed: {after}"
    );
    assert!(
        after.contains("this.title = name;"),
        "expected setter body field reference to be renamed: {after}"
    );
    assert!(
        after.contains("p.getTitle();"),
        "expected getter call site to be renamed: {after}"
    );
    assert!(
        after.contains("p.setTitle(\"x\");"),
        "expected setter call site to be renamed: {after}"
    );
    assert!(
        !after.contains("getName()") && !after.contains("setName("),
        "expected old accessor names to be gone: {after}"
    );
}

#[test]
fn rename_field_does_not_rename_unrelated_java_bean_methods() {
    let file = FileId::new("Test.java");
    let src = r#"class Person {
  private String other;

  public String getName() { return "x"; }

  void m() {
    Person p = new Person();
    p.getName();
    System.out.println(other);
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("String other").unwrap() + "String ".len() + 1;
    let symbol = db.symbol_at(&file, offset).expect("symbol at field other");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "title".into(),
        },
    )
    .unwrap();
    let after = apply_text_edits(src, &edit.text_edits).unwrap();

    assert!(
        after.contains("private String title;"),
        "expected field to be renamed: {after}"
    );
    assert!(
        after.contains("System.out.println(title);"),
        "expected field usage to be renamed: {after}"
    );
    assert!(
        after.contains("getName()"),
        "expected unrelated method to remain unchanged: {after}"
    );
    assert!(
        after.contains("p.getName();"),
        "expected unrelated call site to remain unchanged: {after}"
    );
    assert!(
        !after.contains("getTitle()"),
        "expected no new accessor methods to be introduced: {after}"
    );
}
