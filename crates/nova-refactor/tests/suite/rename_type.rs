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
            new_name: "Widget".into(),
        },
    )
    .unwrap();

    let out = apply_workspace_edit(&files, &edit).unwrap();
    let updated = out.get(&file).unwrap();

    assert!(
        updated.contains("class Widget"),
        "expected declaration rename: {updated}"
    );
    assert!(
        updated.contains("Widget.Inner"),
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

#[test]
fn rename_type_updates_type_import_on_demand_owner() {
    let outer_file = FileId::new("com/example/Outer.java");
    let use_file = FileId::new("Use.java");

    let outer_src = r#"package com.example;

class Outer {
  static class Inner {}
}
"#;

    let use_src = r#"import com.example.Outer.*;

class Use { Inner x; }
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
        updated_use.contains("import com.example.NewOuter.*;"),
        "expected type-import-on-demand to update: {updated_use}"
    );
}

#[test]
fn rename_type_can_be_invoked_from_enclosing_qualifier_in_nested_type_usage() {
    let outer_file = FileId::new("Outer.java");
    let use_file = FileId::new("Use.java");

    let outer_src = r#"class Outer { static class Inner {} }
"#;
    let use_src = r#"class Use { Outer.Inner x; }
"#;

    let mut files = BTreeMap::new();
    files.insert(outer_file.clone(), outer_src.to_string());
    files.insert(use_file.clone(), use_src.to_string());

    let offset = use_src.find("Outer.Inner").unwrap() + 1; // within `Outer`
    let edit = rename_type(
        &files,
        RenameTypeParams {
            file: use_file.clone(),
            offset,
            new_name: "NewOuter".into(),
        },
    )
    .unwrap();

    let out = apply_workspace_edit(&files, &edit).unwrap();
    let updated_outer = out.get(&outer_file).unwrap();
    let updated_use = out.get(&use_file).unwrap();

    assert!(updated_outer.contains("class NewOuter"), "{updated_outer}");
    assert!(
        updated_use.contains("class Use { NewOuter.Inner x; }"),
        "{updated_use}"
    );
}

#[test]
fn rename_type_can_be_invoked_from_qualified_name_expression_in_static_method_call() {
    let outer_file = FileId::new("Outer.java");
    let use_file = FileId::new("Use.java");

    let outer_src = r#"class Outer {
  static class Inner { static void m() {} }
}
"#;
    let use_src = r#"class Use { void f(){ Outer.Inner.m(); } }
"#;

    let mut files = BTreeMap::new();
    files.insert(outer_file.clone(), outer_src.to_string());
    files.insert(use_file.clone(), use_src.to_string());

    let offset = use_src.find("Outer.Inner.m").unwrap() + 1; // within `Outer`
    let edit = rename_type(
        &files,
        RenameTypeParams {
            file: use_file.clone(),
            offset,
            new_name: "NewOuter".into(),
        },
    )
    .unwrap();

    let out = apply_workspace_edit(&files, &edit).unwrap();
    let updated_use = out.get(&use_file).unwrap();

    assert!(
        updated_use.contains("void f(){ NewOuter.Inner.m(); }"),
        "{updated_use}"
    );
    assert!(
        !updated_use.contains("void f(){ Outer.Inner.m();"),
        "{updated_use}"
    );
}

#[test]
fn rename_type_does_not_rename_local_variable_qualifier_in_method_call() {
    let file = FileId::new("Test.java");
    let src = r#"class Foo {}

class Use {
  void f() {
    Foo Foo = new Foo();
    Foo.toString();
  }
}
"#;

    let mut files = BTreeMap::new();
    files.insert(file.clone(), src.to_string());

    let offset = src.find("class Foo").unwrap() + "class ".len() + 1;
    let edit = rename_type(
        &files,
        RenameTypeParams {
            file: file.clone(),
            offset,
            new_name: "Bar".into(),
        },
    )
    .unwrap();

    let out = apply_workspace_edit(&files, &edit).unwrap();
    let updated = out.get(&file).unwrap();

    assert!(updated.contains("class Bar"), "{updated}");
    assert!(updated.contains("Bar Foo = new Bar();"), "{updated}");

    // The `Foo` in `Foo.toString()` is the local variable, not a type reference.
    assert!(updated.contains("Foo.toString();"), "{updated}");
    assert!(!updated.contains("Bar.toString();"), "{updated}");
}

#[test]
fn rename_type_updates_array_initializer() {
    let file = FileId::new("Test.java");
    let src = r#"class Outer {}

class Use {
  Outer[] xs = { new Outer() };
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
            new_name: "Widget".into(),
        },
    )
    .unwrap();

    let out = apply_workspace_edit(&files, &edit).unwrap();
    let updated = out.get(&file).unwrap();

    assert!(updated.contains("class Widget"), "{updated}");
    assert!(updated.contains("Widget[] xs"), "{updated}");
    assert!(updated.contains("new Widget()"), "{updated}");
    assert!(!updated.contains("Outer[] xs"), "{updated}");
    assert!(!updated.contains("new Outer()"), "{updated}");
}
