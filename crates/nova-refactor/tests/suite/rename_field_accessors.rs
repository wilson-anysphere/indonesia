use std::collections::BTreeMap;

use nova_refactor::{apply_workspace_edit, rename, FileId, RefactorJavaDatabase, RenameParams};

#[test]
fn rename_field_renames_accessors_and_call_sites() {
    let foo_file = FileId::new("Foo.java");
    let use_file = FileId::new("Use.java");

    let foo_src = r#"class Foo {
  private String name;

  public String getName() { return name; }

  public void setName(String name) { this.name = name; }

  public void demo() {
    System.out.println(getName());
    setName("name");
    // getName should not change in comment
    String s = "getName name";
  }
}
"#;

    let use_src = r#"class Use {
  void m() {
    Foo foo = new Foo();
    foo.setName("name");
    System.out.println(foo.getName());
    // foo.getName should not change in comment
    String s = "foo.getName name";
  }
}
"#;

    let db = RefactorJavaDatabase::new([
        (foo_file.clone(), foo_src.to_string()),
        (use_file.clone(), use_src.to_string()),
    ]);

    let offset = foo_src.find("String name").unwrap() + "String ".len() + 1;
    let symbol = db.symbol_at(&foo_file, offset).expect("symbol at field");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "fullName".into(),
        },
    )
    .unwrap();

    let files = BTreeMap::from([
        (foo_file.clone(), foo_src.to_string()),
        (use_file.clone(), use_src.to_string()),
    ]);
    let updated = apply_workspace_edit(&files, &edit).unwrap();

    let foo_after = updated.get(&foo_file).expect("updated Foo.java");
    let use_after = updated.get(&use_file).expect("updated Use.java");

    // Field declaration + references.
    assert!(foo_after.contains("String fullName;"), "{foo_after}");
    assert!(foo_after.contains("return fullName;"), "{foo_after}");
    assert!(foo_after.contains("this.fullName = name;"), "{foo_after}");

    // Accessor declarations.
    assert!(foo_after.contains("getFullName()"), "{foo_after}");
    assert!(foo_after.contains("setFullName("), "{foo_after}");

    // Call sites in the same class (unqualified).
    assert!(foo_after.contains("println(getFullName());"), "{foo_after}");
    assert!(foo_after.contains("setFullName(\"name\");"), "{foo_after}");

    // Call sites in another file (qualified).
    assert!(
        use_after.contains("foo.setFullName(\"name\");"),
        "{use_after}"
    );
    assert!(use_after.contains("foo.getFullName()"), "{use_after}");

    // Strings/comments should not be touched.
    assert!(
        foo_after.contains("// getName should not change in comment"),
        "{foo_after}"
    );
    assert!(
        foo_after.contains("String s = \"getName name\";"),
        "{foo_after}"
    );
    assert!(
        use_after.contains("// foo.getName should not change in comment"),
        "{use_after}"
    );
    assert!(
        use_after.contains("String s = \"foo.getName name\";"),
        "{use_after}"
    );
}
