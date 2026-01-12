use nova_refactor::{
    apply_workspace_edit, rename, Conflict, FileId, RefactorJavaDatabase, RenameParams,
    SemanticRefactorError,
};
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;

fn workspace_files() -> (BTreeMap<FileId, String>, FileId, FileId, FileId) {
    let base_file = FileId::new("Base.java");
    let derived_file = FileId::new("Derived.java");
    let use_file = FileId::new("Use.java");

    let base_src = r#"class Base {
  void process(){}
}
"#;

    let derived_src = r#"class Derived extends Base {
  @Override void process(){}
}
"#;

    let use_src = r#"class Use {
  void m(){
    new Base().process();
    new Derived().process();
  }
}
"#;

    let mut files = BTreeMap::new();
    files.insert(base_file.clone(), base_src.to_string());
    files.insert(derived_file.clone(), derived_src.to_string());
    files.insert(use_file.clone(), use_src.to_string());

    (files, base_file, derived_file, use_file)
}

#[test]
fn rename_method_renames_overrides_from_base() {
    let (files, base_file, derived_file, use_file) = workspace_files();
    let db = RefactorJavaDatabase::new(files.clone().into_iter());

    let base_src = files.get(&base_file).unwrap();
    let offset = base_src.find("process").unwrap() + 1;
    let symbol = db
        .symbol_at(&base_file, offset)
        .expect("Base.process symbol");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "handle".into(),
        },
    )
    .unwrap();

    let updated = apply_workspace_edit(&files, &edit).expect("workspace edit applies");

    let updated_base = updated.get(&base_file).unwrap();
    let updated_derived = updated.get(&derived_file).unwrap();
    let updated_use = updated.get(&use_file).unwrap();

    assert!(
        updated_base.contains("void handle()"),
        "expected Base method decl to be renamed:\n{updated_base}"
    );
    assert!(
        updated_derived.contains("void handle()"),
        "expected Derived override decl to be renamed:\n{updated_derived}"
    );
    assert!(
        updated_use.contains("new Base().handle();"),
        "expected Base call site updated:\n{updated_use}"
    );
    assert!(
        updated_use.contains("new Derived().handle();"),
        "expected Derived call site updated:\n{updated_use}"
    );

    assert_eq!(updated_use.contains("process"), false);
}

#[test]
fn rename_method_renames_overrides_from_derived() {
    let (files, base_file, derived_file, use_file) = workspace_files();
    let db = RefactorJavaDatabase::new(files.clone().into_iter());

    let derived_src = files.get(&derived_file).unwrap();
    let offset = derived_src.find("process").unwrap() + 1;
    let symbol = db
        .symbol_at(&derived_file, offset)
        .expect("Derived.process symbol");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "handle".into(),
        },
    )
    .unwrap();

    let updated = apply_workspace_edit(&files, &edit).expect("workspace edit applies");

    let updated_base = updated.get(&base_file).unwrap();
    let updated_derived = updated.get(&derived_file).unwrap();
    let updated_use = updated.get(&use_file).unwrap();

    assert!(
        updated_base.contains("void handle()"),
        "expected Base method decl to be renamed:\n{updated_base}"
    );
    assert!(
        updated_derived.contains("void handle()"),
        "expected Derived override decl to be renamed:\n{updated_derived}"
    );
    assert!(
        updated_use.contains("new Base().handle();"),
        "expected Base call site updated:\n{updated_use}"
    );
    assert!(
        updated_use.contains("new Derived().handle();"),
        "expected Derived call site updated:\n{updated_use}"
    );
}

#[test]
fn rename_method_override_chain_detects_collisions_in_overrides() {
    let base_file = FileId::new("Base.java");
    let derived_file = FileId::new("Derived.java");

    let base_src = r#"class Base {
  void process(){}
}
"#;

    // Derived already defines `handle()`, so renaming `process()` -> `handle()` should conflict
    // once the override chain is included in the rename.
    let derived_src = r#"class Derived extends Base {
  @Override void process(){}
  void handle(){}
}
"#;

    let mut files = BTreeMap::new();
    files.insert(base_file.clone(), base_src.to_string());
    files.insert(derived_file.clone(), derived_src.to_string());

    let db = RefactorJavaDatabase::new(files.clone().into_iter());

    let offset = base_src.find("process").unwrap() + 1;
    let symbol = db.symbol_at(&base_file, offset).expect("Base.process symbol");

    let err = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "handle".into(),
        },
    )
    .unwrap_err();

    let SemanticRefactorError::Conflicts(conflicts) = err else {
        panic!("expected conflict error, got: {err:?}");
    };
    assert!(
        conflicts.iter().any(|c| matches!(
            c,
            Conflict::NameCollision { file, name, .. }
                if file == &derived_file && name == "handle"
        )),
        "expected NameCollision in Derived, got: {conflicts:?}"
    );
}
