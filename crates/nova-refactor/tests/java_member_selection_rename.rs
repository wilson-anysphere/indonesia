use nova_refactor::{apply_text_edits, rename, FileId, RefactorJavaDatabase, RenameParams};

#[test]
fn rename_field_updates_this_field_access() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  int foo;

  void m() {
    this.foo = 1;
  }
}
"#;
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

    assert!(
        after.contains("int bar;"),
        "expected field decl renamed: {after}"
    );
    assert!(
        after.contains("this.bar = 1;"),
        "expected field access renamed: {after}"
    );
    assert!(
        !after.contains("foo"),
        "expected foo to be fully renamed: {after}"
    );
}

#[test]
fn rename_field_updates_obj_field_access_with_declared_type() {
    let file = FileId::new("Test.java");
    let src = r#"class Foo {
  int foo;
}

class Use {
  void m() {
    Foo obj = null;
    obj.foo = 1;
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("int foo").unwrap() + "int ".len() + 1;
    let symbol = db.symbol_at(&file, offset).expect("symbol at Foo.foo");

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
        after.contains("int bar;"),
        "expected field decl renamed: {after}"
    );
    assert!(
        after.contains("obj.bar = 1;"),
        "expected field access renamed: {after}"
    );
}

#[test]
fn rename_method_updates_this_method_call() {
    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void foo() {}

  void m() {
    this.foo();
  }
}
"#;
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

    assert!(
        after.contains("void bar()"),
        "expected method decl renamed: {after}"
    );
    assert!(
        after.contains("this.bar();"),
        "expected method call renamed: {after}"
    );
    assert!(!after.contains("foo()"), "expected old name gone: {after}");
}

#[test]
fn rename_method_updates_obj_method_call_with_declared_type() {
    let file = FileId::new("Test.java");
    let src = r#"class Foo {
  void foo() {}
}

class Use {
  void m() {
    Foo obj = null;
    obj.foo();
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("void foo").unwrap() + "void ".len() + 1;
    let symbol = db
        .symbol_at(&file, offset)
        .expect("symbol at Foo.foo method");

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
        after.contains("void bar()"),
        "expected method decl renamed: {after}"
    );
    assert!(
        after.contains("obj.bar();"),
        "expected method call renamed: {after}"
    );
}

#[test]
fn rename_method_updates_method_reference() {
    let file = FileId::new("Test.java");
    let src = r#"class Foo {
  static void foo() {}
}

class Use {
  void m() {
    Runnable r = Foo::foo;
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("void foo").unwrap() + "void ".len() + 1;
    let symbol = db
        .symbol_at(&file, offset)
        .expect("symbol at Foo.foo method");

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
        after.contains("static void bar()"),
        "expected method decl renamed: {after}"
    );
    assert!(
        after.contains("Foo::bar"),
        "expected method reference renamed: {after}"
    );
}

#[test]
fn rename_static_field_updates_qualified_field_access() {
    let file = FileId::new("Test.java");
    let src = r#"class Foo {
  static int SFOO;
}

class Use {
  void m() {
    int x = Foo.SFOO;
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("static int SFOO").unwrap() + "static int ".len() + 1;
    let symbol = db.symbol_at(&file, offset).expect("symbol at Foo.SFOO");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "SBAR".into(),
        },
    )
    .unwrap();
    let after = apply_text_edits(src, &edit.text_edits).unwrap();

    assert!(
        after.contains("static int SBAR;"),
        "expected static field decl renamed: {after}"
    );
    assert!(
        after.contains("Foo.SBAR"),
        "expected qualified field access renamed: {after}"
    );
    assert!(!after.contains("SFOO"), "expected old name gone: {after}");
}

#[test]
fn rename_static_method_updates_qualified_method_call() {
    let file = FileId::new("Test.java");
    let src = r#"class Foo {
  static void foo() {}
}

class Use {
  void m() {
    Foo.foo();
  }
}
"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("static void foo").unwrap() + "static void ".len() + 1;
    let symbol = db
        .symbol_at(&file, offset)
        .expect("symbol at Foo.foo method");

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
        after.contains("static void bar()"),
        "expected static method decl renamed: {after}"
    );
    assert!(
        after.contains("Foo.bar();"),
        "expected qualified method call renamed: {after}"
    );
    assert!(!after.contains("foo()"), "expected old name gone: {after}");
}
