use nova_refactor::{apply_text_edits, rename, FileId, RefactorJavaDatabase, RenameParams};

#[test]
fn rename_type_parameter_recursive_bound_updates_all_uses() {
    let file = FileId::new("Test.java");
    let src = r#"class Test<T extends Comparable<T>> {
  T f;
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("<T extends").unwrap() + "<".len();
    let symbol = db
        .symbol_at(&file, offset)
        .expect("symbol at type parameter T");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "U".into(),
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    let expected = r#"class Test<U extends Comparable<U>> {
  U f;
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn rename_type_parameter_multiple_bounds_updates_all_uses() {
    let file = FileId::new("Test.java");
    let src = r#"class Test<T extends java.io.Serializable & Comparable<T>> {
  T f;
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("<T extends").unwrap() + "<".len();
    let symbol = db
        .symbol_at(&file, offset)
        .expect("symbol at type parameter T");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "U".into(),
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    let expected = r#"class Test<U extends java.io.Serializable & Comparable<U>> {
  U f;
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn rename_type_parameter_updates_wildcard_bounds() {
    let file = FileId::new("Test.java");
    let src = r#"class Test<T> {
  java.util.List<? extends T> xs;
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("<T>").unwrap() + "<".len();
    let symbol = db
        .symbol_at(&file, offset)
        .expect("symbol at type parameter T");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "U".into(),
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    let expected = r#"class Test<U> {
  java.util.List<? extends U> xs;
}
"#;
    assert_eq!(after, expected);
}

#[test]
fn rename_type_parameter_updates_explicit_type_arguments_in_method_call() {
    let file = FileId::new("Test.java");
    let src = r#"class Test<T> {
  <U> void g() {}
  void f() { this.<T>g(); }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("<T>").unwrap() + "<".len();
    let symbol = db
        .symbol_at(&file, offset)
        .expect("symbol at type parameter T");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "X".into(),
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    let expected = r#"class Test<X> {
  <U> void g() {}
  void f() { this.<X>g(); }
}
"#;
    assert_eq!(after, expected);
}

