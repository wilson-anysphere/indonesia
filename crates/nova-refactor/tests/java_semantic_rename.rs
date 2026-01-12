use std::collections::BTreeMap;

use nova_refactor::{
    apply_workspace_edit, rename, FileId, JavaSymbolKind, RefactorJavaDatabase, RenameParams,
};

#[test]
fn rename_field_updates_member_accesses_across_files() {
    let file_a = FileId::new("A.java");
    let file_b = FileId::new("B.java");

    let src_a = r#"package p;
class A {
  int foo;

  void m() {
    this.foo = 1;
  }
}
"#;

    let src_b = r#"package p;
class B {
  void m() {
    A obj = new A();
    int x = obj.foo;
  }
}
"#;

    let db = RefactorJavaDatabase::new([
        (file_a.clone(), src_a.to_string()),
        (file_b.clone(), src_b.to_string()),
    ]);

    let offset = src_a.find("int foo").unwrap() + "int ".len();
    let symbol = db.symbol_at(&file_a, offset).expect("field symbol");
    assert_eq!(db.symbol_kind(symbol), Some(JavaSymbolKind::Field));

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "bar".to_string(),
        },
    )
    .expect("rename succeeds");

    let mut files = BTreeMap::new();
    files.insert(file_a.clone(), src_a.to_string());
    files.insert(file_b.clone(), src_b.to_string());
    let out = apply_workspace_edit(&files, &edit).expect("edit applies");

    let out_a = out.get(&file_a).expect("A.java updated");
    assert!(out_a.contains("int bar;"));
    assert!(out_a.contains("this.bar = 1;"));

    let out_b = out.get(&file_b).expect("B.java updated");
    assert!(out_b.contains("int x = obj.bar;"));
}

#[test]
fn rename_method_updates_calls_across_files() {
    let file_a = FileId::new("A.java");
    let file_b = FileId::new("B.java");

    let src_a = r#"package p;
class A {
  void bar() {}

  void m() {
    this.bar();
  }
}
"#;

    let src_b = r#"package p;
class B {
  void m() {
    A obj = new A();
    obj.bar();
  }
}
"#;

    let db = RefactorJavaDatabase::new([
        (file_a.clone(), src_a.to_string()),
        (file_b.clone(), src_b.to_string()),
    ]);

    let offset = src_a.find("void bar").unwrap() + "void ".len();
    let symbol = db.symbol_at(&file_a, offset).expect("method symbol");
    assert_eq!(db.symbol_kind(symbol), Some(JavaSymbolKind::Method));

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "baz".to_string(),
        },
    )
    .expect("rename succeeds");

    let mut files = BTreeMap::new();
    files.insert(file_a.clone(), src_a.to_string());
    files.insert(file_b.clone(), src_b.to_string());
    let out = apply_workspace_edit(&files, &edit).expect("edit applies");

    let out_a = out.get(&file_a).expect("A.java updated");
    assert!(out_a.contains("void baz()"));
    assert!(out_a.contains("this.baz();"));

    let out_b = out.get(&file_b).expect("B.java updated");
    assert!(out_b.contains("obj.baz();"));
}

#[test]
fn rename_type_updates_constructor_and_usages_across_files() {
    let file_a = FileId::new("A.java");
    let file_b = FileId::new("B.java");

    let src_a = r#"package p;
class A {
  A() {}
}
"#;

    let src_b = r#"package p;
class B {
  void m() {
    A obj = new A();
  }
}
"#;

    let db = RefactorJavaDatabase::new([
        (file_a.clone(), src_a.to_string()),
        (file_b.clone(), src_b.to_string()),
    ]);

    let offset = src_a.find("class A").unwrap() + "class ".len();
    let symbol = db.symbol_at(&file_a, offset).expect("type symbol");
    assert_eq!(db.symbol_kind(symbol), Some(JavaSymbolKind::Type));

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "C".to_string(),
        },
    )
    .expect("rename succeeds");

    let mut files = BTreeMap::new();
    files.insert(file_a.clone(), src_a.to_string());
    files.insert(file_b.clone(), src_b.to_string());
    let out = apply_workspace_edit(&files, &edit).expect("edit applies");

    let out_a = out.get(&file_a).expect("A.java updated");
    assert!(out_a.contains("class C"));
    assert!(out_a.contains("C() {}"));

    let out_b = out.get(&file_b).expect("B.java updated");
    assert!(out_b.contains("C obj = new C();"));
}
