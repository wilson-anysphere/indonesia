use nova_refactor::{
    apply_text_edits, materialize, FileId, JavaSymbolKind, RefactorJavaDatabase, SemanticChange,
};

#[test]
fn rename_annotation_method_updates_annotation_element_name() {
    let file = FileId::new("Test.java");
    let src = r#"@interface Foo { int bar(); }

@Foo(bar = 1)
class Use {}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("int bar").unwrap() + "int ".len() + 1;
    let symbol = db.symbol_at(&file, offset).expect("symbol at bar()");

    assert_eq!(db.symbol_kind(symbol), Some(JavaSymbolKind::Method));

    let edit = materialize(
        &db,
        [SemanticChange::Rename {
            symbol,
            new_name: "baz".into(),
        }],
    )
    .unwrap();
    let after = apply_text_edits(src, &edit.text_edits).unwrap();

    assert!(after.contains("int baz();"), "method declaration renamed: {after}");
    assert!(
        after.contains("@Foo(baz = 1)"),
        "annotation usage element name renamed: {after}"
    );
    assert!(
        !after.contains("@Foo(bar = 1)"),
        "old element name should be gone: {after}"
    );
}

#[test]
fn renaming_non_annotation_method_does_not_touch_annotation_element_names() {
    let file = FileId::new("Test.java");
    let src = r#"@interface Foo { int bar(); }

class Other {
  void bar() {}
}

@Foo(bar = 1)
class Use {}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("void bar").unwrap() + "void ".len() + 1;
    let symbol = db.symbol_at(&file, offset).expect("symbol at Other.bar()");

    assert_eq!(db.symbol_kind(symbol), Some(JavaSymbolKind::Method));

    let edit = materialize(
        &db,
        [SemanticChange::Rename {
            symbol,
            new_name: "baz".into(),
        }],
    )
    .unwrap();
    let after = apply_text_edits(src, &edit.text_edits).unwrap();

    assert!(after.contains("void baz()"), "method declaration renamed: {after}");
    assert!(
        after.contains("@Foo(bar = 1)"),
        "annotation element name should remain unchanged: {after}"
    );
    assert!(
        after.contains("int bar();"),
        "annotation method should remain unchanged: {after}"
    );
}

