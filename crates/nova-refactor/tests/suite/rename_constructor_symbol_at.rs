use std::collections::BTreeMap;

use nova_refactor::{
    apply_workspace_edit, rename, FileId, FileOp, JavaSymbolKind, RefactorJavaDatabase, RenameParams,
};
use pretty_assertions::assert_eq;

#[test]
fn symbol_at_constructor_name_returns_type_symbol() {
    let file = FileId::new("Foo.java");
    let src = r#"package p; public class Foo { public Foo() {} }"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("public Foo()").unwrap() + "public ".len() + 1;
    let symbol = db
        .symbol_at(&file, offset)
        .expect("symbol at constructor name");

    assert_eq!(db.symbol_kind(symbol), Some(JavaSymbolKind::Type));
}

#[test]
fn symbol_at_constructor_name_resolves_to_owning_type_symbol() {
    let file = FileId::new("Test.java");
    let src = r#"class Outer { Outer() {} class Inner { Inner() {} } }"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let outer_decl_offset = src.find("class Outer").unwrap() + "class ".len() + 1;
    let outer_symbol = db
        .symbol_at(&file, outer_decl_offset)
        .expect("symbol at outer type name");
    assert_eq!(db.symbol_kind(outer_symbol), Some(JavaSymbolKind::Type));

    let outer_ctor_offset = src.find("Outer()").unwrap() + 1;
    let outer_ctor_symbol = db
        .symbol_at(&file, outer_ctor_offset)
        .expect("symbol at outer constructor name");
    assert_eq!(outer_ctor_symbol, outer_symbol);

    let inner_decl_offset = src.find("class Inner").unwrap() + "class ".len() + 1;
    let inner_symbol = db
        .symbol_at(&file, inner_decl_offset)
        .expect("symbol at inner type name");
    assert_eq!(db.symbol_kind(inner_symbol), Some(JavaSymbolKind::Type));

    let inner_ctor_offset = src.find("Inner()").unwrap() + 1;
    let inner_ctor_symbol = db
        .symbol_at(&file, inner_ctor_offset)
        .expect("symbol at inner constructor name");
    assert_eq!(inner_ctor_symbol, inner_symbol);
    assert_ne!(inner_symbol, outer_symbol);
}

#[test]
fn rename_from_constructor_name_renames_file_when_public_top_level_type() {
    let file = FileId::new("Foo.java");
    let src = r#"package p;

public class Foo {
  public Foo() {}

  void m() {
    Foo x = new Foo();
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("public Foo()").unwrap() + "public ".len() + 1;
    let symbol = db
        .symbol_at(&file, offset)
        .expect("symbol at constructor name");
    assert_eq!(db.symbol_kind(symbol), Some(JavaSymbolKind::Type));

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "Bar".into(),
        },
    )
    .unwrap();

    let renamed_file = FileId::new("Bar.java");
    assert_eq!(
        edit.file_ops,
        vec![FileOp::Rename {
            from: file.clone(),
            to: renamed_file.clone(),
        }]
    );

    let mut files = BTreeMap::new();
    files.insert(file.clone(), src.to_string());
    let out = apply_workspace_edit(&files, &edit).unwrap();

    assert!(!out.contains_key(&file), "expected file to be renamed");
    let updated = out
        .get(&renamed_file)
        .expect("expected renamed file to exist");

    assert!(
        updated.contains("public class Bar"),
        "expected type declaration rename: {updated}"
    );
    assert!(
        updated.contains("public Bar()"),
        "expected constructor rename: {updated}"
    );
    assert!(
        updated.contains("Bar x = new Bar()"),
        "expected new/type references to be renamed: {updated}"
    );
    assert!(!updated.contains("Foo"), "expected Foo to be fully renamed");
}
