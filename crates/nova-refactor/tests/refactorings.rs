use nova_refactor::{
    apply_text_edits, extract_variable, inline_variable, rename, Conflict, ExtractVariableParams,
    FileId, InlineVariableParams, RefactorDatabase, RefactorJavaDatabase, RenameParams,
    SemanticRefactorError, WorkspaceTextRange,
};
use pretty_assertions::assert_eq;

mod suite;

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
        },
    )
    .unwrap_err();
    assert!(
        matches!(err, SemanticRefactorError::ExtractNotSupported { .. }),
        "expected ExtractNotSupported error, got: {err:?}"
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
