use nova_refactor::{
    apply_text_edits, extract_variable, rename, Conflict, ExtractVariableParams, FileId,
    RefactorJavaDatabase, RenameParams, SemanticRefactorError, WorkspaceTextRange,
};
use pretty_assertions::assert_eq;

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
