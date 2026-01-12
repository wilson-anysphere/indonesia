use nova_refactor::{
    apply_text_edits, extract_variable, inline_variable, rename, Conflict, ExtractVariableParams,
    FileId, InlineVariableParams, JavaSymbolKind, RefactorDatabase, RefactorJavaDatabase,
    RenameParams, SemanticRefactorError, WorkspaceTextRange,
};
use nova_test_utils::extract_range;
use pretty_assertions::assert_eq;

mod suite;

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
fn extract_variable_replaces_whole_expression_statement() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  static class Foo {}

  void m() {
    new Foo();
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let expr_start = src.find("new Foo()").unwrap();
    let expr_end = expr_start + "new Foo()".len();

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
  static class Foo {}

  void m() {
    Foo result = new Foo();
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
    String x = /*start*/foo()/*end*/;
  }

  String foo() { return ""; }
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
    String y = foo();
    String x = y;
  }

  String foo() { return ""; }
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn extract_variable_prefers_typeck_when_it_adds_generics() {
    let file = FileId::new("Test.java");
    let fixture = r#"import java.util.ArrayList;
import java.util.List;

class Test {
  void m() {
    List<String> xs = /*select*/new ArrayList<>()/*end*/;
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
    let expected = r#"import java.util.ArrayList;
import java.util.List;

class Test {
  void m() {
    ArrayList<String> tmp = new ArrayList<>();
    List<String> xs = tmp;
  }
}
"#;
    assert_eq!(after, expected);
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
    Foo x = /*select*/new Foo()/*end*/;
  }
}
"#,
            r#"class Foo {}

class Test {
  void m() {
    Foo value = new Foo();
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

    assert!(
        matches!(
            err,
            SemanticRefactorError::ExtractNotSupported { reason }
                if reason == "extracted variable name conflicts with an existing binding"
        ),
        "expected lambda parameter name conflict, got: {err:?}"
    );
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

    assert!(
        matches!(
            err,
            SemanticRefactorError::ExtractNotSupported { reason }
                if reason == "extracted variable name conflicts with an existing binding"
        ),
        "expected catch parameter name conflict, got: {err:?}"
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
                if reason == "cannot extract from switch expression rule body"
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
                if reason == "cannot extract from switch expression rule body"
        ),
        "expected switch expression rule rejection, got: {err:?}"
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
                if reason == "cannot extract from switch expression rule body"
        ),
        "expected switch expression rule rejection, got: {err:?}"
    );
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
        matches!(
            err,
            SemanticRefactorError::ExtractNotSupported { reason }
                if reason == "expression has side effects and cannot be extracted safely"
        ),
        "expected side-effect ExtractNotSupported error, got: {err:?}"
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
        matches!(
            err,
            SemanticRefactorError::ExtractNotSupported { reason }
                if reason == "expression has side effects and cannot be extracted safely"
        ),
        "expected side-effect ExtractNotSupported error, got: {err:?}"
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
        "expected assert ExtractNotSupportedInAssert error, got: {err:?}"
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
        "expected assert ExtractNotSupportedInAssert error, got: {err:?}"
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
fn inline_variable_rejected_when_initializer_dependency_is_written_before_use() {
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
      case 1:
        int a = 1 + 2;
        System.out.println(a);
        break;
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
      case 1:
        System.out.println((1 + 2));
        break;
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
fn inline_variable_rejects_side_effectful_initializer_with_intervening_statement() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  int foo() { return 1; }
  void bar() {}
  void m() {
    int a = foo();
    bar();
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
fn inline_variable_allows_side_effectful_initializer_when_usage_is_immediately_next_statement() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  int foo() { return 1; }
  void m() {
    int a = foo();
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
  int foo() { return 1; }
  void m() {
    System.out.println(foo());
  }
}
"#;
    assert_eq!(after, expected);
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

    assert!(matches!(
        err,
        SemanticRefactorError::ExtractNotSupportedInAssert
    ));
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

    assert!(matches!(
        err,
        SemanticRefactorError::ExtractNotSupportedInAssert
    ));
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
