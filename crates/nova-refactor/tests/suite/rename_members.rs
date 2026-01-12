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
    assert!(
        after.contains("class Derived extends Base { int y;"),
        "{after}"
    );
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
fn rename_annotation_value_element_skips_other_annotation_types_with_same_simple_name() {
    let a_p = FileId::new("p/A.java");
    let a_q = FileId::new("q/A.java");
    let use_file = FileId::new("Use.java");

    let src_p = r#"package p;
public @interface A {
    int value();
}
"#;
    let src_q = r#"package q;
public @interface A {
    int value();
}
"#;
    let src_use = r#"package use;
import p.A;

@A(1)
@q.A(2)
class Use {}
"#;

    let db = RefactorJavaDatabase::new([
        (a_p.clone(), src_p.to_string()),
        (a_q.clone(), src_q.to_string()),
        (use_file.clone(), src_use.to_string()),
    ]);

    let offset = src_p.find("int value").unwrap() + "int ".len() + 1;
    let symbol = db
        .symbol_at(&a_p, offset)
        .expect("symbol at p.A annotation element value()");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "v".into(),
        },
    )
    .unwrap();

    let mut files = BTreeMap::new();
    files.insert(a_p.clone(), src_p.to_string());
    files.insert(a_q.clone(), src_q.to_string());
    files.insert(use_file.clone(), src_use.to_string());
    let out = apply_workspace_edit(&files, &edit).unwrap();

    let after_p = out.get(&a_p).unwrap();
    let after_q = out.get(&a_q).unwrap();
    let after_use = out.get(&use_file).unwrap();

    assert!(after_p.contains("int v();"), "{after_p}");
    assert!(after_q.contains("int value();"), "{after_q}");

    assert!(after_use.contains("@A(v = 1)"), "{after_use}");
    assert!(after_use.contains("@q.A(2)"), "{after_use}");
    assert!(!after_use.contains("@q.A(v = 2)"), "{after_use}");
    assert!(!after_use.contains("@q.A(value = 2)"), "{after_use}");
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

#[test]
fn rename_type_updates_imports_fields_and_method_signatures() {
    let foo = FileId::new("Foo.java");
    let use_ = FileId::new("Use.java");

    let src_foo = r#"package p;
class Foo { Foo(){} }"#;

    let src_use = r#"package q;
import p.Foo;

class Use {
    Foo field;
    Foo m(Foo param) {
        Foo local = new Foo();
        return local;
    }
}"#;

    let db = RefactorJavaDatabase::new([
        (foo.clone(), src_foo.to_string()),
        (use_.clone(), src_use.to_string()),
    ]);

    let offset = src_foo.find("class Foo").unwrap() + "class ".len() + 1;
    let symbol = db.symbol_at(&foo, offset).expect("symbol at type Foo");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "Bar".into(),
        },
    )
    .unwrap();

    let mut files = BTreeMap::new();
    files.insert(foo.clone(), src_foo.to_string());
    files.insert(use_.clone(), src_use.to_string());

    let out = apply_workspace_edit(&files, &edit).unwrap();
    let after_foo = out.get(&foo).unwrap();
    let after_use = out.get(&use_).unwrap();

    assert!(after_foo.contains("class Bar"), "{after_foo}");
    assert!(after_foo.contains("Bar()"), "{after_foo}");

    assert!(after_use.contains("import p.Bar;"), "{after_use}");
    assert!(after_use.contains("Bar field;"), "{after_use}");
    assert!(after_use.contains("Bar m(Bar param)"), "{after_use}");
    assert!(after_use.contains("Bar local = new Bar()"), "{after_use}");
    assert!(!after_use.contains("Foo"), "{after_use}");
}

#[test]
fn rename_type_updates_module_info_uses_and_provides_directives() {
    let module_info = FileId::new("module-info.java");
    let foo = FileId::new("Foo.java");

    let src_module = r#"module m {
  uses Foo;
  provides Foo;
}
"#;
    let src_foo = r#"class Foo {}"#;

    let db = RefactorJavaDatabase::new([
        (module_info.clone(), src_module.to_string()),
        (foo.clone(), src_foo.to_string()),
    ]);

    let offset = src_foo.find("class Foo").unwrap() + "class ".len() + 1;
    let symbol = db.symbol_at(&foo, offset).expect("symbol at type Foo");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "Bar".into(),
        },
    )
    .unwrap();

    let mut files = BTreeMap::new();
    files.insert(module_info.clone(), src_module.to_string());
    files.insert(foo.clone(), src_foo.to_string());

    let out = apply_workspace_edit(&files, &edit).unwrap();
    let after_module = out.get(&module_info).unwrap();

    assert!(after_module.contains("uses Bar;"), "{after_module}");
    assert!(after_module.contains("provides Bar;"), "{after_module}");
    assert!(!after_module.contains("uses Foo;"), "{after_module}");
    assert!(!after_module.contains("provides Foo;"), "{after_module}");
}

#[test]
fn rename_nested_type_outer_segment_is_renamed() {
    let file = FileId::new("Test.java");
    let src = r#"class Outer {
    static class Inner {
        Inner() {}
    }
}

class Use {
    Outer.Inner field;
    void m() {
        new Outer.Inner();
    }
}"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("Outer.Inner field").unwrap() + 1;
    let symbol = db
        .symbol_at(&file, offset)
        .expect("symbol at Outer segment in Outer.Inner");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "Top".into(),
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    assert!(after.contains("class Top"), "{after}");
    assert!(after.contains("Top.Inner field"), "{after}");
    assert!(after.contains("new Top.Inner()"), "{after}");
    assert!(!after.contains("Outer."), "{after}");
}

#[test]
fn rename_nested_type_inner_segment_is_renamed() {
    let file = FileId::new("Test.java");
    let src = r#"class Outer {
    static class Inner {
        Inner() {}
    }
}

class Use {
    Outer.Inner field;
    void m() {
        new Outer.Inner();
    }
}"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("Outer.Inner field").unwrap() + "Outer.".len() + 1;
    let symbol = db
        .symbol_at(&file, offset)
        .expect("symbol at Inner segment in Outer.Inner");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "RenamedInner".into(),
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    assert!(after.contains("class RenamedInner"), "{after}");
    assert!(after.contains("RenamedInner()"), "{after}");
    assert!(after.contains("Outer.RenamedInner field"), "{after}");
    assert!(after.contains("new Outer.RenamedInner()"), "{after}");
    assert!(!after.contains(".Inner"), "{after}");
}

#[test]
fn rename_method_overloads_renames_all_declarations_and_calls() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
    void foo() {}
    void foo(int x) {}
    void m() { foo(); foo(1); }
}"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("void foo()").unwrap() + "void ".len() + 1;
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
    assert!(after.contains("void bar(int x)"), "{after}");
    assert!(after.contains("bar();"), "{after}");
    assert!(after.contains("bar(1);"), "{after}");
    assert!(!after.contains("foo"), "{after}");
}

#[test]
fn rename_record_component_updates_compact_constructor_parameter_references_from_header() {
    let file = FileId::new("Test.java");
    let src = r#"record R(int x) {
    R {
        System.out.println(x);
    }
}

class Use {
    void m() {
        R r = new R(1);
        System.out.println(r.x());
        System.out.println(new R(2).x());
    }
}"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("record R(int x").unwrap() + "record R(int ".len();
    let symbol = db
        .symbol_at(&file, offset)
        .expect("symbol at record component x");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "y".into(),
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    assert!(after.contains("record R(int y)"), "{after}");
    assert!(after.contains("println(y)"), "{after}");
    assert!(after.contains("r.y()"), "{after}");
    assert!(after.contains("new R(2).y()"), "{after}");
    assert!(!after.contains("println(x)"), "{after}");
    assert!(!after.contains(".x()"), "{after}");
    assert!(!after.contains("int x"), "{after}");
}

#[test]
fn rename_record_component_updates_compact_constructor_parameter_references_from_constructor_body()
{
    let file = FileId::new("Test.java");
    let src = r#"record R(int x) {
    R {
        System.out.println(x);
    }
}

class Use {
    void m() {
        R r = new R(1);
        System.out.println(r.x());
        System.out.println(new R(2).x());
    }
}"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("println(x)").unwrap() + "println(".len();
    let symbol = db
        .symbol_at(&file, offset)
        .expect("symbol at record component x reference in compact constructor body");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "y".into(),
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    assert!(after.contains("record R(int y)"), "{after}");
    assert!(after.contains("println(y)"), "{after}");
    assert!(after.contains("r.y()"), "{after}");
    assert!(after.contains("new R(2).y()"), "{after}");
    assert!(!after.contains("println(x)"), "{after}");
    assert!(!after.contains(".x()"), "{after}");
    assert!(!after.contains("int x"), "{after}");
}
