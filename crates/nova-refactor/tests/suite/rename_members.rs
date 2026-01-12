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
fn rename_field_updates_inherited_references_in_subclass() {
    let file = FileId::new("Test.java");
    let src = r#"class Base { int x; } class Derived extends Base { void f(){ x = 1; this.x = 2; super.x = 3; } }"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int x").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at Base.x");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "y".into(),
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    assert!(after.contains("int y;"), "{after}");
    assert!(after.contains("y = 1"), "{after}");
    assert!(after.contains("this.y = 2"), "{after}");
    assert!(after.contains("super.y = 3"), "{after}");
    assert!(!after.contains("int x"), "{after}");
    assert!(!after.contains("x = 1"), "{after}");
    assert!(!after.contains("this.x"), "{after}");
    assert!(!after.contains("super.x"), "{after}");
}

#[test]
fn rename_field_updates_super_reference_even_when_shadowed() {
    let file = FileId::new("Test.java");
    let src =
        r#"class Base { int x; } class Derived extends Base { int y; void f(){ super.x = 1; } }"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int x").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file, offset).expect("symbol at Base.x");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "y".into(),
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    assert!(after.contains("class Base { int y; }"), "{after}");
    assert!(after.contains("class Derived extends Base { int y;"), "{after}");
    assert!(after.contains("super.y = 1"), "{after}");
    assert!(!after.contains("super.x"), "{after}");
}

#[test]
fn rename_method_updates_declaration_and_call() {
    let file = FileId::new("Test.java");
    let src = r#"class Test { void foo(){} void m(){ foo(); } }"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("void foo").unwrap() + "void ".len() + 1;
    let symbol = db.symbol_at(&file, offset).expect("symbol at method foo");

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
fn rename_annotation_value_element_rewrites_shorthand_usages() {
    let file = FileId::new("Test.java");
    let src = r#"@interface A {
    int value();
}

@A(1)
class Use {
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int value").unwrap() + "int ".len() + 1;
    let symbol = db
        .symbol_at(&file, offset)
        .expect("symbol at annotation element value()");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "v".into(),
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    assert!(after.contains("int v();"), "{after}");
    assert!(after.contains("@A(v = 1)"), "{after}");
    assert!(!after.contains("@A(1)"), "{after}");
}

#[test]
fn rename_annotation_value_element_rewrites_named_value_pairs() {
    let file = FileId::new("Test.java");
    let src = r#"@interface A {
    int value();
}

@A(value = 1)
class Use {
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int value").unwrap() + "int ".len() + 1;
    let symbol = db
        .symbol_at(&file, offset)
        .expect("symbol at annotation element value()");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "v".into(),
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    assert!(after.contains("int v();"), "{after}");
    assert!(after.contains("@A(v = 1)"), "{after}");
    assert!(!after.contains("@A(value = 1)"), "{after}");
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

#[test]
fn rename_type_preserves_generics_in_local_and_new_expression() {
    let file = FileId::new("Test.java");
    let src = r#"class Foo<T> { Foo(){} }

class Use {
    void m() {
        Foo<String> x = new Foo<>();
    }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("class Foo").unwrap() + "class ".len() + 1;
    let symbol = db.symbol_at(&file, offset).expect("symbol at type Foo");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "Bar".into(),
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    assert!(after.contains("class Bar"), "{after}");
    assert!(after.contains("Bar()"), "{after}");
    assert!(after.contains("Bar<String> x"), "{after}");
    assert!(after.contains("new Bar<>()"), "{after}");
    assert!(!after.contains("Foo"), "{after}");
}

#[test]
fn rename_type_updates_fully_qualified_new_expression_simple_name_only() {
    let a = FileId::new("A.java");
    let b = FileId::new("B.java");

    let src_a = r#"package pkg;
class Foo { Foo(){} }"#;
    let src_b = r#"class Use { void m(){ new pkg.Foo(); } }"#;

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
    assert!(after_b.contains("new pkg.Bar()"), "{after_b}");
    assert!(!after_a.contains("Foo"), "{after_a}");
    assert!(!after_b.contains("pkg.Foo"), "{after_b}");
}
