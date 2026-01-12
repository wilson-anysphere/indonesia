use std::collections::BTreeMap;

use nova_refactor::{
    apply_text_edits, apply_workspace_edit, rename, FileId, RefactorJavaDatabase, RenameParams,
};

#[test]
fn rename_field_updates_reference_in_another_field_initializer() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  int foo = 1;
  int bar = foo + 1;
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    // Trigger rename from the reference site inside the field initializer.
    let offset = src.find("foo +").unwrap() + 1;
    let symbol = db.symbol_at(&file, offset).expect("symbol at foo");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "baz".into(),
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    assert!(
        after.contains("int baz = 1;"),
        "expected field declaration renamed"
    );
    assert!(
        after.contains("int bar = baz + 1;"),
        "expected initializer reference renamed"
    );
}

#[test]
fn rename_method_updates_call_in_field_initializer() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  int x = compute();

  int compute() {
    return 1;
  }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    // Trigger rename from the call site inside the field initializer.
    let offset = src.find("compute();").unwrap() + 1;
    let symbol = db.symbol_at(&file, offset).expect("symbol at compute()");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "calc".into(),
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    assert!(after.contains("int x = calc();"));
    assert!(after.contains("int calc()"));
}

#[test]
fn rename_enum_constant_updates_switch_case_label() {
    let enum_file = FileId::new("E.java");
    let switch_file = FileId::new("Test.java");

    let enum_src = r#"enum E { FOO }
"#;
    let switch_src = r#"class Test {
  void m(E e) {
    switch (e) {
      case FOO:
        break;
    }
  }
}
"#;

    let db = RefactorJavaDatabase::new([
        (enum_file.clone(), enum_src.to_string()),
        (switch_file.clone(), switch_src.to_string()),
    ]);

    // Trigger rename from the case label (implicit enum constant reference).
    let offset = switch_src.find("case FOO").unwrap() + "case ".len() + 1;
    let symbol = db
        .symbol_at(&switch_file, offset)
        .expect("symbol at enum case label");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "BAR".into(),
        },
    )
    .unwrap();

    let mut files = BTreeMap::new();
    files.insert(enum_file.clone(), enum_src.to_string());
    files.insert(switch_file.clone(), switch_src.to_string());

    let out = apply_workspace_edit(&files, &edit).unwrap();
    assert!(
        out.get(&enum_file).unwrap().contains("enum E { BAR }"),
        "expected enum constant declaration renamed"
    );
    assert!(
        out.get(&switch_file).unwrap().contains("case BAR:"),
        "expected switch label renamed"
    );
}
