use nova_refactor::{
    apply_text_edits, inline_variable, FileId, InlineVariableParams, RefactorJavaDatabase,
    SemanticRefactorError,
};

#[test]
fn inline_variable_rejects_shadowing_of_field_dependency_in_nested_block() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  int b = 1;
  void m() {
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
fn inline_variable_rejects_shadowing_of_field_dependency_by_for_header_declaration() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  int b = 1;
  void m() {
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
fn inline_variable_rejects_shadowing_of_field_dependency_by_catch_parameter() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  int b = 1;
  void m() {
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
fn inline_variable_rejects_shadowing_of_field_dependency_by_try_resource() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  int b = 1;
  void m() {
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
fn inline_variable_rejects_shadowing_of_field_dependency_by_pattern_variable() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  int b = 1;
  void m(Object o) {
    int a = b;
    if (o instanceof Integer b) {
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
fn inline_variable_allows_inlining_field_dependency_when_not_shadowed() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  int b = 1;
  void m() {
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
        after.contains("System.out.println(b);"),
        "expected `a` to be inlined to `b`, got: {after}"
    );
    assert!(
        !after.contains("int a ="),
        "expected `a` declaration to be removed, got: {after}"
    );
}
