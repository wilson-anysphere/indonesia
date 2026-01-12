use std::collections::BTreeMap;

use nova_refactor::{
    apply_text_edits, apply_workspace_edit, materialize, FileId, JavaSymbolKind,
    RefactorJavaDatabase, SemanticChange,
};

#[test]
fn type_rename_updates_class_literal_in_annotation() {
    let file = FileId::new("Test.java");
    let src = r#"@interface Anno { Class<?> value(); }

@Anno(Foo.class)
class Use {}

class Foo {}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("class Foo").unwrap() + "class ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at Foo");
    assert_eq!(db.symbol_kind(symbol), Some(JavaSymbolKind::Type));

    let edit = materialize(
        &db,
        [SemanticChange::Rename {
            symbol,
            new_name: "Bar".into(),
        }],
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    assert!(
        after.contains("@Anno(Bar.class)"),
        "expected annotation updated: {after}"
    );
    assert!(
        after.contains("class Bar"),
        "expected type declaration updated: {after}"
    );
    assert!(
        !after.contains("Foo.class"),
        "expected old class literal removed: {after}"
    );
}

#[test]
fn enum_constant_rename_updates_annotation_expression() {
    let file = FileId::new("Test.java");
    let src = r#"@interface Anno { MyEnum value(); }

enum MyEnum { FOO, BAR }

@Anno(MyEnum.FOO)
class Use {}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("FOO,").unwrap();
    let symbol = db
        .symbol_at(&file, offset)
        .expect("symbol at enum constant FOO");
    assert_eq!(db.symbol_kind(symbol), Some(JavaSymbolKind::Field));

    let edit = materialize(
        &db,
        [SemanticChange::Rename {
            symbol,
            new_name: "BAZ".into(),
        }],
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    assert!(
        after.contains("enum MyEnum { BAZ, BAR }"),
        "expected enum constant declaration updated: {after}"
    );
    assert!(
        after.contains("@Anno(MyEnum.BAZ)"),
        "expected annotation expression updated: {after}"
    );
}

#[test]
fn static_field_rename_updates_annotation_value_with_static_import() {
    let consts = FileId::new("p/Consts.java");
    let use_file = FileId::new("p/Use.java");

    let consts_src = r#"package p;

public class Consts {
  public static final int CONST = 1;
}
"#;

    let use_src = r#"package p;

import static p.Consts.CONST;

@interface Anno {
  int value();
}

@Anno(value = CONST)
public class Use {}
"#;

    let db = RefactorJavaDatabase::new([
        (consts.clone(), consts_src.to_string()),
        (use_file.clone(), use_src.to_string()),
    ]);

    let offset = consts_src.find("CONST =").unwrap();
    let symbol = db
        .symbol_at(&consts, offset)
        .expect("symbol at CONST field");
    assert_eq!(db.symbol_kind(symbol), Some(JavaSymbolKind::Field));

    let edit = materialize(
        &db,
        [SemanticChange::Rename {
            symbol,
            new_name: "NEW_CONST".into(),
        }],
    )
    .unwrap();

    let files = BTreeMap::from([
        (consts.clone(), consts_src.to_string()),
        (use_file.clone(), use_src.to_string()),
    ]);
    let after_files = apply_workspace_edit(&files, &edit).unwrap();

    let after_consts = after_files.get(&consts).unwrap();
    assert!(
        after_consts.contains("NEW_CONST = 1"),
        "expected field declaration updated: {after_consts}"
    );

    let after_use = after_files.get(&use_file).unwrap();
    assert!(
        after_use.contains("import static p.Consts.NEW_CONST;"),
        "expected static import updated: {after_use}"
    );
    assert!(
        after_use.contains("@Anno(value = NEW_CONST)"),
        "expected annotation value updated: {after_use}"
    );
}

#[test]
fn type_rename_updates_new_expression_in_enum_constant_args() {
    let file = FileId::new("Test.java");
    let src = r#"class Foo {}

enum E {
  A(new Foo());
  E(Foo foo) {}
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("class Foo").unwrap() + "class ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at Foo");
    assert_eq!(db.symbol_kind(symbol), Some(JavaSymbolKind::Type));

    let edit = materialize(
        &db,
        [SemanticChange::Rename {
            symbol,
            new_name: "Bar".into(),
        }],
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    assert!(
        after.contains("new Bar()"),
        "expected enum constant argument updated: {after}"
    );
    assert!(
        after.contains("class Bar"),
        "expected type declaration updated: {after}"
    );
    assert!(
        !after.contains("new Foo()"),
        "expected old enum constant argument removed: {after}"
    );
}

#[test]
fn method_rename_updates_call_in_enum_constant_args() {
    let file = FileId::new("Test.java");
    let src = r#"enum E {
  A(bar());
  E(int x) {}
  static int bar() { return 1; }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("static int bar").unwrap() + "static int ".len() + 1;
    let symbol = db.symbol_at(&file, offset).expect("symbol at bar method");
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
    assert!(
        after.contains("A(baz())"),
        "expected enum constant argument call updated: {after}"
    );
    assert!(
        after.contains("static int baz()"),
        "expected method declaration updated: {after}"
    );
    assert!(
        !after.contains("bar()"),
        "expected old method name removed: {after}"
    );
}
