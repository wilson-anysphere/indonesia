use std::collections::BTreeMap;

use nova_refactor::{
    apply_workspace_edit, rename, FileId, FileOp, JavaSymbolKind, RefactorJavaDatabase,
    RenameParams,
};

#[test]
fn rename_type_updates_references_and_renames_file() {
    let foo_file = FileId::new("p/Foo.java");
    let use_file = FileId::new("q/Use.java");

    let foo_src = "package p; public class Foo { Foo() {} }";
    let use_src = "import p.Foo; class Use { Foo f; void m(){ new Foo(); } }";

    let db = RefactorJavaDatabase::new([
        (foo_file.clone(), foo_src.to_string()),
        (use_file.clone(), use_src.to_string()),
    ]);

    let offset = foo_src.find("class Foo").unwrap() + "class ".len() + 1;
    let symbol = db.symbol_at(&foo_file, offset).expect("type symbol at Foo");
    assert_eq!(db.symbol_kind(symbol), Some(JavaSymbolKind::Type));

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "Bar".into(),
        },
    )
    .unwrap();

    let bar_file = FileId::new("p/Bar.java");
    assert!(
        edit.file_ops.contains(&FileOp::Rename {
            from: foo_file.clone(),
            to: bar_file.clone(),
        }),
        "expected file rename op: {:?}",
        edit.file_ops
    );

    let mut files = BTreeMap::new();
    files.insert(foo_file.clone(), foo_src.to_string());
    files.insert(use_file.clone(), use_src.to_string());

    let out = apply_workspace_edit(&files, &edit).unwrap();

    assert!(
        !out.contains_key(&foo_file),
        "expected Foo.java to be renamed away"
    );
    let foo_after = out.get(&bar_file).expect("Bar.java exists");
    assert!(foo_after.contains("public class Bar"));
    assert!(foo_after.contains("Bar()"));

    let use_after = out.get(&use_file).expect("Use.java exists");
    assert!(use_after.contains("import p.Bar;"));
    assert!(use_after.contains("Bar f;"));
    assert!(use_after.contains("new Bar();"));
}

#[test]
fn rename_type_does_not_rename_local_variable_named_same() {
    let foo_file = FileId::new("p/Foo.java");
    let use_file = FileId::new("q/Use.java");

    let foo_src = "package p; public class Foo { Foo() {} }";
    let use_src =
        "import p.Foo; class Use { void m(){ Foo Foo = new Foo(); System.out.println(Foo); } }";

    let db = RefactorJavaDatabase::new([
        (foo_file.clone(), foo_src.to_string()),
        (use_file.clone(), use_src.to_string()),
    ]);

    let offset = foo_src.find("class Foo").unwrap() + "class ".len() + 1;
    let symbol = db.symbol_at(&foo_file, offset).expect("type symbol at Foo");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "Bar".into(),
        },
    )
    .unwrap();

    let mut files = BTreeMap::new();
    files.insert(foo_file.clone(), foo_src.to_string());
    files.insert(use_file.clone(), use_src.to_string());

    let out = apply_workspace_edit(&files, &edit).unwrap();
    let use_after = out.get(&use_file).expect("Use.java exists");

    assert!(
        use_after.contains("Bar Foo = new Bar();"),
        "expected type refs to be renamed but local variable name preserved: {use_after}"
    );
    assert!(
        use_after.contains("println(Foo);"),
        "expected local variable usage to remain unchanged: {use_after}"
    );
}
