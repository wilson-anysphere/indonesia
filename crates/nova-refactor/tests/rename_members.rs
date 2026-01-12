use std::collections::BTreeMap;

use nova_refactor::{
    apply_text_edits, apply_workspace_edit, rename, FileId, RefactorJavaDatabase, RenameParams,
};

#[test]
fn rename_field_updates_declaration_and_usage() {
    let file = FileId::new("Test.java");
    let src = r#"class Test { int foo; void m(){ foo = 1; } }"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int foo").unwrap() + "int ".len() + 1;
    let symbol = db.symbol_at(&file, offset).expect("symbol at field foo");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "bar".into(),
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    assert!(after.contains("int bar;"), "{after}");
    assert!(after.contains("bar = 1"), "{after}");
    assert!(!after.contains("foo"), "{after}");
}

#[test]
fn rename_field_updates_this_qualified_reference() {
    let file = FileId::new("Test.java");
    let src = r#"class Test { int foo; void m(){ this.foo = 1; } }"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("this.foo").unwrap() + "this.".len() + 1;
    let symbol = db
        .symbol_at(&file, offset)
        .expect("symbol at this.foo reference");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "bar".into(),
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    assert!(after.contains("int bar;"), "{after}");
    assert!(after.contains("this.bar = 1"), "{after}");
    assert!(!after.contains("foo"), "{after}");
}

#[test]
fn rename_method_updates_declaration_and_call() {
    let file = FileId::new("Test.java");
    let src = r#"class Test { void foo(){} void m(){ foo(); } }"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("void foo").unwrap() + "void ".len() + 1;
    let symbol = db
        .symbol_at(&file, offset)
        .expect("symbol at method foo");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "bar".into(),
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    assert!(after.contains("void bar()"), "{after}");
    assert!(after.contains("bar();"), "{after}");
    assert!(!after.contains("foo"), "{after}");
}

#[test]
fn rename_method_updates_this_qualified_call() {
    let file = FileId::new("Test.java");
    let src = r#"class Test { void foo(){} void m(){ this.foo(); } }"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("this.foo").unwrap() + "this.".len() + 1;
    let symbol = db
        .symbol_at(&file, offset)
        .expect("symbol at this.foo() call");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "bar".into(),
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    assert!(after.contains("void bar()"), "{after}");
    assert!(after.contains("this.bar();"), "{after}");
    assert!(!after.contains("foo"), "{after}");
}

#[test]
fn rename_type_updates_declaration_constructor_and_cross_file_new() {
    let a = FileId::new("A.java");
    let b = FileId::new("B.java");

    let src_a = r#"class Foo { Foo(){} }"#;
    let src_b = r#"class Use { void m(){ new Foo(); } }"#;

    let db = RefactorJavaDatabase::new([
        (a.clone(), src_a.to_string()),
        (b.clone(), src_b.to_string()),
    ]);

    let offset = src_a.find("class Foo").unwrap() + "class ".len() + 1;
    let symbol = db.symbol_at(&a, offset).expect("symbol at type Foo");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "Bar".into(),
        },
    )
    .unwrap();

    let mut files = BTreeMap::new();
    files.insert(a.clone(), src_a.to_string());
    files.insert(b.clone(), src_b.to_string());

    let out = apply_workspace_edit(&files, &edit).unwrap();
    let after_a = out.get(&a).unwrap();
    let after_b = out.get(&b).unwrap();

    assert!(after_a.contains("class Bar"), "{after_a}");
    assert!(after_a.contains("Bar()"), "{after_a}");
    assert!(after_b.contains("new Bar()"), "{after_b}");
    assert!(!after_a.contains("Foo"), "{after_a}");
    assert!(!after_b.contains("Foo"), "{after_b}");
}
