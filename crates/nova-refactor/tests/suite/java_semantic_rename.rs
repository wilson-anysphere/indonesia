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

#[test]
fn rename_type_updates_module_info_uses_and_provides_directives() {
    let module_info = FileId::new("module-info.java");
    let service_file = FileId::new("p/Service.java");
    let impl_file = FileId::new("p/impl/ServiceImpl.java");

    let src_module = r#"module m {
  uses p.Service;
  provides p.Service with p.impl.ServiceImpl;
}
"#;

    let src_service = r#"package p;
public interface Service {}
"#;

    let src_impl = r#"package p.impl;
public class ServiceImpl implements p.Service {}
"#;

    let db = RefactorJavaDatabase::new([
        (module_info.clone(), src_module.to_string()),
        (service_file.clone(), src_service.to_string()),
        (impl_file.clone(), src_impl.to_string()),
    ]);

    let offset = src_service.find("interface Service").unwrap() + "interface ".len();
    let symbol = db
        .symbol_at(&service_file, offset)
        .expect("service type symbol");
    assert_eq!(db.symbol_kind(symbol), Some(JavaSymbolKind::Type));

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "NewService".to_string(),
        },
    )
    .expect("rename succeeds");

    let mut files = BTreeMap::new();
    files.insert(module_info.clone(), src_module.to_string());
    files.insert(service_file.clone(), src_service.to_string());
    files.insert(impl_file.clone(), src_impl.to_string());
    let out = apply_workspace_edit(&files, &edit).expect("edit applies");

    let out_module = out.get(&module_info).expect("module-info updated");
    assert!(out_module.contains("uses p.NewService;"), "{out_module}");
    assert!(
        out_module.contains("provides p.NewService with p.impl.ServiceImpl;"),
        "{out_module}"
    );
}

#[test]
fn rename_type_updates_module_info_provides_with_clause() {
    let module_info = FileId::new("module-info.java");
    let service_file = FileId::new("p/Service.java");
    let impl_file = FileId::new("p/impl/ServiceImpl.java");

    let src_module = r#"module m {
  uses p.Service;
  provides p.Service with p.impl.ServiceImpl;
}
"#;

    let src_service = r#"package p;
public interface Service {}
"#;

    let src_impl = r#"package p.impl;
public class ServiceImpl implements p.Service {}
"#;

    let db = RefactorJavaDatabase::new([
        (module_info.clone(), src_module.to_string()),
        (service_file.clone(), src_service.to_string()),
        (impl_file.clone(), src_impl.to_string()),
    ]);

    let offset = src_impl.find("class ServiceImpl").unwrap() + "class ".len();
    let symbol = db
        .symbol_at(&impl_file, offset)
        .expect("implementation type symbol");
    assert_eq!(db.symbol_kind(symbol), Some(JavaSymbolKind::Type));

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "NewServiceImpl".to_string(),
        },
    )
    .expect("rename succeeds");

    let mut files = BTreeMap::new();
    files.insert(module_info.clone(), src_module.to_string());
    files.insert(service_file.clone(), src_service.to_string());
    files.insert(impl_file.clone(), src_impl.to_string());
    let out = apply_workspace_edit(&files, &edit).expect("edit applies");

    let out_module = out.get(&module_info).expect("module-info updated");
    assert!(
        out_module.contains("provides p.Service with p.impl.NewServiceImpl;"),
        "{out_module}"
    );
}
