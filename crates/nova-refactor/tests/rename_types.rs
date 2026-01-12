use std::collections::BTreeMap;

use nova_refactor::{
    apply_workspace_edit, rename, FileId, RefactorJavaDatabase, RenameParams, WorkspaceEdit,
};

#[test]
fn rename_type_updates_imports_annotations_and_type_positions() {
    let foo_file = FileId::new("Foo.java");
    let bar_file = FileId::new("Bar.java");
    let use_file = FileId::new("Use.java");

    let foo_src = r#"package p;
public class Foo {}
"#;

    let use_src = r#"package q;
import p.Foo;

class Use {
  Foo f;
  java.util.List<Foo> xs;
  @Foo int x;

  void m() {
    Foo y = new Foo();
    String s = "Foo"; // Foo
  }
}
"#;

    let db = RefactorJavaDatabase::new([
        (foo_file.clone(), foo_src.to_string()),
        (use_file.clone(), use_src.to_string()),
    ]);

    let offset = foo_src.find("class Foo").unwrap() + "class ".len() + 1;
    let symbol = db.symbol_at(&foo_file, offset).expect("symbol at Foo");

    let edit: WorkspaceEdit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "Bar".into(),
        },
    )
    .unwrap();

    let mut files: BTreeMap<FileId, String> = BTreeMap::new();
    files.insert(foo_file.clone(), foo_src.to_string());
    files.insert(use_file.clone(), use_src.to_string());
    let updated = apply_workspace_edit(&files, &edit).unwrap();

    assert!(
        !updated.contains_key(&foo_file),
        "expected Foo.java to be renamed away"
    );

    let foo_after = updated.get(&bar_file).expect("updated Bar.java");
    let use_after = updated.get(&use_file).expect("updated Use.java");

    assert!(foo_after.contains("class Bar"), "{foo_after}");
    assert!(!foo_after.contains("class Foo"), "{foo_after}");

    assert!(use_after.contains("import p.Bar;"), "{use_after}");
    assert!(!use_after.contains("import p.Foo;"), "{use_after}");

    assert!(use_after.contains("Bar f;"), "{use_after}");
    assert!(use_after.contains("java.util.List<Bar> xs;"), "{use_after}");
    assert!(use_after.contains("@Bar int x;"), "{use_after}");
    assert!(use_after.contains("Bar y = new Bar();"), "{use_after}");

    // Strings/comments are not semantic references.
    assert!(use_after.contains("String s = \"Foo\";"), "{use_after}");
    assert!(use_after.contains("// Foo"), "{use_after}");
    assert!(!use_after.contains("\"Bar\""), "{use_after}");
}
