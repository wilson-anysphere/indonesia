use std::collections::BTreeMap;

use nova_refactor::{apply_workspace_edit, rename_type, FileId, RenameTypeParams};

#[test]
fn rename_type_updates_enclosing_qualifier_in_nested_type_usage() {
    let file = FileId::new("Test.java");
    let src = r#"class Outer { static class Inner {} }
class Use { Outer.Inner x; }
"#;

    let mut files = BTreeMap::new();
    files.insert(file.clone(), src.to_string());

    let offset = src.find("class Outer").unwrap() + "class ".len() + 1;
    let edit = rename_type(
        &files,
        RenameTypeParams {
            file: file.clone(),
            offset,
            new_name: "NewOuter".into(),
        },
    )
    .unwrap();

    let out = apply_workspace_edit(&files, &edit).unwrap();
    let updated = out.get(&file).unwrap();

    assert!(
        updated.contains("class NewOuter"),
        "expected declaration rename: {updated}"
    );
    assert!(
        updated.contains("NewOuter.Inner"),
        "expected qualified usage rename: {updated}"
    );
    assert!(
        !updated.contains("class Use { Outer.Inner"),
        "expected old qualified usage to be gone: {updated}"
    );
}

#[test]
fn rename_type_updates_qualifier_in_fully_qualified_nested_type() {
    let outer_file = FileId::new("com/example/Outer.java");
    let use_file = FileId::new("Use.java");

    let outer_src = r#"package com.example;

class Outer {
  static class Inner {}
}
"#;

    let use_src = r#"class Use { com.example.Outer.Inner x; }
"#;

    let mut files = BTreeMap::new();
    files.insert(outer_file.clone(), outer_src.to_string());
    files.insert(use_file.clone(), use_src.to_string());

    let offset = outer_src.find("class Outer").unwrap() + "class ".len() + 1;
    let edit = rename_type(
        &files,
        RenameTypeParams {
            file: outer_file.clone(),
            offset,
            new_name: "NewOuter".into(),
        },
    )
    .unwrap();

    let out = apply_workspace_edit(&files, &edit).unwrap();
    let updated_outer = out.get(&outer_file).unwrap();
    let updated_use = out.get(&use_file).unwrap();

    assert!(
        updated_outer.contains("class NewOuter"),
        "expected declaration rename: {updated_outer}"
    );
    assert!(
        updated_use.contains("com.example.NewOuter.Inner"),
        "expected fully-qualified usage to update qualifier: {updated_use}"
    );
    assert!(
        !updated_use.contains("com.example.Outer.Inner"),
        "expected old fully-qualified usage to be gone: {updated_use}"
    );
}

#[test]
fn rename_type_updates_static_import_owner_chain() {
    let outer_file = FileId::new("com/example/Outer.java");
    let use_file = FileId::new("Use.java");

    let outer_src = r#"package com.example;

class Outer {
  static class Inner {
    static final int CONST = 1;
  }
}
"#;

    let use_src = r#"import static com.example.Outer.Inner.CONST;

class Use { int x = CONST; }
"#;

    let mut files = BTreeMap::new();
    files.insert(outer_file.clone(), outer_src.to_string());
    files.insert(use_file.clone(), use_src.to_string());

    let offset = outer_src.find("class Outer").unwrap() + "class ".len() + 1;
    let edit = rename_type(
        &files,
        RenameTypeParams {
            file: outer_file.clone(),
            offset,
            new_name: "NewOuter".into(),
        },
    )
    .unwrap();

    let out = apply_workspace_edit(&files, &edit).unwrap();
    let updated_use = out.get(&use_file).unwrap();

    assert!(
        updated_use.contains("import static com.example.NewOuter.Inner.CONST;"),
        "expected static import to update: {updated_use}"
    );
}
