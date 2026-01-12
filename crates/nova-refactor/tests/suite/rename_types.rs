use std::collections::BTreeMap;

use nova_refactor::{
    apply_workspace_edit, rename, FileId, FileOp, RefactorJavaDatabase, RenameParams, WorkspaceEdit,
};

fn renamed_file(edit: &WorkspaceEdit, original: &FileId) -> FileId {
    edit.file_ops
        .iter()
        .find_map(|op| match op {
            FileOp::Rename { from, to } if from == original => Some(to.clone()),
            _ => None,
        })
        .unwrap_or_else(|| original.clone())
}

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

    // `rename` performs an optional file rename for public top-level types when the file name
    // matches the type name (`Foo.java` -> `Bar.java`).
    assert!(
        !updated.contains_key(&foo_file),
        "expected Foo.java to be renamed to Bar.java"
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

#[test]
fn rename_nested_type_updates_static_import() {
    let outer_file = FileId::new("p/Outer.java");
    let use_file = FileId::new("q/Use.java");

    let outer_src = r#"package p;
public class Outer {
  public static class Inner {}
}
"#;

    let use_src = r#"package q;
import static p.Outer.Inner;

class Use {
  Inner x;
}
"#;

    let db = RefactorJavaDatabase::new([
        (outer_file.clone(), outer_src.to_string()),
        (use_file.clone(), use_src.to_string()),
    ]);

    let offset = outer_src.find("class Inner").unwrap() + "class ".len() + 1;
    let symbol = db
        .symbol_at(&outer_file, offset)
        .expect("symbol at nested type Inner");

    let edit: WorkspaceEdit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "RenamedInner".into(),
        },
    )
    .unwrap();

    let mut files: BTreeMap<FileId, String> = BTreeMap::new();
    files.insert(outer_file.clone(), outer_src.to_string());
    files.insert(use_file.clone(), use_src.to_string());
    let updated = apply_workspace_edit(&files, &edit).unwrap();

    let outer_after = updated.get(&outer_file).expect("updated Outer.java");
    let use_after = updated.get(&use_file).expect("updated Use.java");

    assert!(
        outer_after.contains("class RenamedInner"),
        "{outer_after}"
    );
    assert!(
        use_after.contains("import static p.Outer.RenamedInner;"),
        "{use_after}"
    );
    assert!(use_after.contains("RenamedInner x;"), "{use_after}");
}

#[test]
fn rename_type_updates_static_wildcard_import_qualifier() {
    let outer_file = FileId::new("p/Outer.java");
    let use_file = FileId::new("q/Use.java");

    let outer_src = r#"package p;
public class Outer {
  public static final int CONST = 1;
}
"#;

    let use_src = r#"package q;
import static p.Outer.*;

class Use {
  int x = CONST;
}
"#;

    let db = RefactorJavaDatabase::new([
        (outer_file.clone(), outer_src.to_string()),
        (use_file.clone(), use_src.to_string()),
    ]);

    let offset = outer_src.find("class Outer").unwrap() + "class ".len() + 1;
    let symbol = db
        .symbol_at(&outer_file, offset)
        .expect("symbol at type Outer");

    let edit: WorkspaceEdit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "NewOuter".into(),
        },
    )
    .unwrap();

    let mut files: BTreeMap<FileId, String> = BTreeMap::new();
    files.insert(outer_file.clone(), outer_src.to_string());
    files.insert(use_file.clone(), use_src.to_string());
    let updated = apply_workspace_edit(&files, &edit).unwrap();

    let outer_file_after = renamed_file(&edit, &outer_file);
    let outer_after = updated.get(&outer_file_after).expect("updated Outer.java");
    let use_after = updated.get(&use_file).expect("updated Use.java");

    assert!(outer_after.contains("class NewOuter"), "{outer_after}");
    assert!(
        use_after.contains("import static p.NewOuter.*;"),
        "{use_after}"
    );
    assert!(!use_after.contains("import static p.Outer.*;"), "{use_after}");
}
