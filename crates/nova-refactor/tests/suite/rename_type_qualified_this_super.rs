use std::collections::BTreeMap;

use nova_refactor::{apply_workspace_edit, rename_type, FileId, RenameTypeParams};

#[test]
fn rename_type_updates_qualified_this_expression() {
    let file = FileId::new("Test.java");
    let src = r#"class Outer { class Inner { void m(){ Outer.this.toString(); } } }"#;

    let mut files = BTreeMap::new();
    files.insert(file.clone(), src.to_string());

    let offset = src.find("class Outer").unwrap() + "class ".len() + 1;
    let edit = rename_type(
        &files,
        RenameTypeParams {
            file: file.clone(),
            offset,
            new_name: "Renamed".into(),
        },
    )
    .unwrap();

    let after_files = apply_workspace_edit(&files, &edit).unwrap();
    let after = after_files.get(&file).unwrap();

    assert!(after.contains("class Renamed"));
    assert!(after.contains("Renamed.this.toString()"));
    assert!(!after.contains("Outer.this"));
}

#[test]
fn rename_type_updates_qualified_super_expression() {
    let file = FileId::new("Test.java");
    let src = r#"class Base {}
class Outer extends Base {
  class Inner {
    void m() { Outer.super.toString(); }
  }
}
"#;

    let mut files = BTreeMap::new();
    files.insert(file.clone(), src.to_string());

    let offset = src.find("class Outer").unwrap() + "class ".len() + 1;
    let edit = rename_type(
        &files,
        RenameTypeParams {
            file: file.clone(),
            offset,
            new_name: "Renamed".into(),
        },
    )
    .unwrap();

    let after_files = apply_workspace_edit(&files, &edit).unwrap();
    let after = after_files.get(&file).unwrap();

    assert!(after.contains("class Renamed extends Base"));
    assert!(after.contains("Renamed.super.toString()"));
    assert!(!after.contains("Outer.super"));
}

#[test]
fn rename_type_can_be_invoked_from_qualified_this_qualifier() {
    let file = FileId::new("Test.java");
    let src = r#"class Outer { class Inner { void m(){ Outer.this.toString(); } } }"#;

    let mut files = BTreeMap::new();
    files.insert(file.clone(), src.to_string());

    let offset = src.find("Outer.this").unwrap() + 1;
    let edit = rename_type(
        &files,
        RenameTypeParams {
            file: file.clone(),
            offset,
            new_name: "Renamed".into(),
        },
    )
    .unwrap();

    let after_files = apply_workspace_edit(&files, &edit).unwrap();
    let after = after_files.get(&file).unwrap();

    assert!(after.contains("class Renamed"));
    assert!(after.contains("Renamed.this.toString()"));
}

#[test]
fn rename_type_can_be_invoked_from_qualified_super_qualifier() {
    let file = FileId::new("Test.java");
    let src = r#"class Base {}
class Outer extends Base {
  class Inner {
    void m() { Outer.super.toString(); }
  }
}
"#;

    let mut files = BTreeMap::new();
    files.insert(file.clone(), src.to_string());

    let offset = src.find("Outer.super").unwrap() + 1;
    let edit = rename_type(
        &files,
        RenameTypeParams {
            file: file.clone(),
            offset,
            new_name: "Renamed".into(),
        },
    )
    .unwrap();

    let after_files = apply_workspace_edit(&files, &edit).unwrap();
    let after = after_files.get(&file).unwrap();

    assert!(after.contains("class Renamed extends Base"));
    assert!(after.contains("Renamed.super.toString()"));
}
