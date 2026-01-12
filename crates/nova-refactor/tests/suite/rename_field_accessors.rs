use std::collections::BTreeMap;

use nova_refactor::{
    apply_workspace_edit, rename, Conflict, FileId, RefactorJavaDatabase, RenameParams,
    SemanticRefactorError,
};

#[test]
fn rename_field_renames_accessors_and_call_sites() {
    let foo_file = FileId::new("Foo.java");
    let use_file = FileId::new("Use.java");

    let foo_src = r#"class Foo {
  private String name;

  public String getName() { return name; }

  public void setName(String name) { this.name = name; }

  // Overloads / unrelated methods should not be renamed (only arity-matching accessors).
  public String getName(int i) { return name + i; }
  public void setName() { this.name = "x"; }

  public void demo() {
    System.out.println(getName());
    System.out.println(getName(1));
    setName("name");
    setName();
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
    assert!(foo_after.contains("return fullName + i;"), "{foo_after}");
    assert!(foo_after.contains("this.fullName = \"x\";"), "{foo_after}");

    // Accessor declarations.
    assert!(foo_after.contains("getFullName()"), "{foo_after}");
    assert!(foo_after.contains("setFullName("), "{foo_after}");
    // Overloads / unrelated methods preserved.
    assert!(foo_after.contains("getName(int i)"), "{foo_after}");
    assert!(foo_after.contains("setName()"), "{foo_after}");

    // Call sites in the same class (unqualified).
    assert!(foo_after.contains("println(getFullName());"), "{foo_after}");
    assert!(foo_after.contains("println(getName(1));"), "{foo_after}");
    assert!(foo_after.contains("setFullName(\"name\");"), "{foo_after}");
    assert!(foo_after.contains("setName();"), "{foo_after}");

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

#[test]
fn rename_field_renames_is_accessor() {
    let foo_file = FileId::new("Foo.java");
    let use_file = FileId::new("Use.java");

    let foo_src = r#"class Foo {
  private boolean active;

  public boolean isActive() { return active; }
  public void setActive(boolean active) { this.active = active; }
}
"#;

    let use_src = r#"class Use {
  boolean m(Foo foo) {
    foo.setActive(true);
    return foo.isActive();
  }
}
"#;

    let db = RefactorJavaDatabase::new([
        (foo_file.clone(), foo_src.to_string()),
        (use_file.clone(), use_src.to_string()),
    ]);

    let offset = foo_src.find("boolean active").unwrap() + "boolean ".len() + 1;
    let symbol = db.symbol_at(&foo_file, offset).expect("symbol at field");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "enabled".into(),
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

    assert!(foo_after.contains("boolean enabled;"), "{foo_after}");
    assert!(foo_after.contains("isEnabled()"), "{foo_after}");
    assert!(foo_after.contains("setEnabled("), "{foo_after}");
    assert!(foo_after.contains("return enabled;"), "{foo_after}");
    assert!(foo_after.contains("this.enabled = active;"), "{foo_after}");

    assert!(use_after.contains("foo.setEnabled(true);"), "{use_after}");
    assert!(use_after.contains("foo.isEnabled();"), "{use_after}");
}

#[test]
fn rename_field_accessor_collision_is_reported() {
    let file = FileId::new("Foo.java");

    let src = r#"class Foo {
  private String name;
  public String getName() { return name; }
  public void setName(String name) { this.name = name; }

  // Pre-existing accessor name should conflict with rename(name -> fullName).
  public String getFullName() { return "already"; }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("String name").unwrap() + "String ".len() + 1;
    let symbol = db.symbol_at(&file, offset).expect("symbol at field");

    let err = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "fullName".into(),
        },
    )
    .unwrap_err();

    let SemanticRefactorError::Conflicts(conflicts) = err else {
        panic!("expected Conflicts, got: {err:?}");
    };

    assert!(
        conflicts.iter().any(|c| matches!(
            c,
            Conflict::NameCollision { name, .. } if name == "getFullName"
        )),
        "expected a NameCollision on getFullName, got: {conflicts:?}"
    );
}
