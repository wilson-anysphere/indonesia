use std::collections::BTreeMap;

use nova_refactor::{apply_workspace_edit, rename, FileId, RefactorJavaDatabase, RenameParams};

#[test]
fn rename_static_imported_field_updates_import_and_usages() {
    let foo_file = FileId::new("src/main/java/p/Foo.java");
    let use_file = FileId::new("src/main/java/q/Use.java");

    let foo_src = r#"package p;

public class Foo {
  public static int CONST = 1;
  public static void oldName(){}
}
"#;

    let use_src = r#"package q;

import static p.Foo.CONST;
import static p.Foo.oldName;

class Use {
  void m() {
    System.out.println(CONST);
    oldName();
  }
}
"#;

    let db = RefactorJavaDatabase::new([
        (foo_file.clone(), foo_src.to_string()),
        (use_file.clone(), use_src.to_string()),
    ]);

    let offset = foo_src.find("CONST").unwrap() + 1;
    let symbol = db.symbol_at(&foo_file, offset).expect("symbol at CONST");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "VALUE".into(),
        },
    )
    .unwrap();

    let mut files = BTreeMap::new();
    files.insert(foo_file.clone(), foo_src.to_string());
    files.insert(use_file.clone(), use_src.to_string());

    let updated = apply_workspace_edit(&files, &edit).unwrap();
    let foo_after = updated.get(&foo_file).unwrap();
    let use_after = updated.get(&use_file).unwrap();

    assert!(
        foo_after.contains("int VALUE = 1"),
        "expected field declaration to be renamed: {foo_after}"
    );
    assert!(
        use_after.contains("import static p.Foo.VALUE;"),
        "expected static import to be updated: {use_after}"
    );
    assert!(
        use_after.contains("println(VALUE)"),
        "expected usage to be renamed: {use_after}"
    );
}

#[test]
fn rename_static_imported_method_updates_import_and_usages() {
    let foo_file = FileId::new("src/main/java/p/Foo.java");
    let use_file = FileId::new("src/main/java/q/Use.java");

    let foo_src = r#"package p;

public class Foo {
  public static int CONST = 1;
  public static void oldName(){}
}
"#;

    let use_src = r#"package q;

import static p.Foo.CONST;
import static p.Foo.oldName;

class Use {
  void m() {
    System.out.println(CONST);
    oldName();
  }
}
"#;

    let db = RefactorJavaDatabase::new([
        (foo_file.clone(), foo_src.to_string()),
        (use_file.clone(), use_src.to_string()),
    ]);

    let offset = foo_src.find("oldName").unwrap() + 1;
    let symbol = db.symbol_at(&foo_file, offset).expect("symbol at oldName");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "newName".into(),
        },
    )
    .unwrap();

    let mut files = BTreeMap::new();
    files.insert(foo_file.clone(), foo_src.to_string());
    files.insert(use_file.clone(), use_src.to_string());

    let updated = apply_workspace_edit(&files, &edit).unwrap();
    let foo_after = updated.get(&foo_file).unwrap();
    let use_after = updated.get(&use_file).unwrap();

    assert!(
        foo_after.contains("void newName()"),
        "expected method declaration to be renamed: {foo_after}"
    );
    assert!(
        use_after.contains("import static p.Foo.newName;"),
        "expected static import to be updated: {use_after}"
    );
    assert!(
        use_after.contains("newName();"),
        "expected invocation to be renamed: {use_after}"
    );
}
