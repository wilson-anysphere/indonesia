use nova_refactor::{
    apply_workspace_edit, rename, Conflict, FileId, RefactorJavaDatabase, RenameParams,
    SemanticRefactorError,
};
use std::collections::BTreeMap;

#[test]
fn rename_method_renames_interface_implementations() {
    let interface_file = FileId::new("I.java");
    let impl_file = FileId::new("C.java");
    let use_file = FileId::new("Use.java");

    let interface_src = r#"interface I {
  void process();
}
"#;

    let impl_src = r#"class C implements I {
  @Override public void process(){}
  void call() {
    process();
  }
}
"#;

    let use_src = r#"class Use {
  void m(I i, C c) {
    i.process();
    c.process();
  }
}
"#;

    let mut files = BTreeMap::new();
    files.insert(interface_file.clone(), interface_src.to_string());
    files.insert(impl_file.clone(), impl_src.to_string());
    files.insert(use_file.clone(), use_src.to_string());

    let db = RefactorJavaDatabase::new(files.clone().into_iter());

    let offset = interface_src.find("process").unwrap() + 1;
    let symbol = db
        .symbol_at(&interface_file, offset)
        .expect("I.process symbol");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "handle".into(),
        },
    )
    .unwrap();

    let updated = apply_workspace_edit(&files, &edit).expect("workspace edit applies");

    let updated_interface = updated.get(&interface_file).unwrap();
    let updated_impl = updated.get(&impl_file).unwrap();
    let updated_use = updated.get(&use_file).unwrap();

    assert!(
        updated_interface.contains("void handle();"),
        "expected interface method renamed:\n{updated_interface}"
    );
    assert!(
        updated_impl.contains("void handle()"),
        "expected implementing method renamed:\n{updated_impl}"
    );
    assert!(
        updated_impl.contains("handle();"),
        "expected unqualified call updated:\n{updated_impl}"
    );
    assert!(
        updated_use.contains("i.handle();"),
        "expected interface-typed call site updated:\n{updated_use}"
    );
    assert!(
        updated_use.contains("c.handle();"),
        "expected class-typed call site updated:\n{updated_use}"
    );

    assert_eq!(updated_use.contains("process"), false);
}

#[test]
fn rename_method_renames_implementations_through_subinterface_chain() {
    let base_file = FileId::new("I.java");
    let child_file = FileId::new("J.java");
    let impl_file = FileId::new("C.java");

    let base_src = r#"interface I {
  void process();
}
"#;

    let child_src = r#"interface J extends I {
}
"#;

    let impl_src = r#"class C implements J {
  @Override public void process(){}
}
"#;

    let mut files = BTreeMap::new();
    files.insert(base_file.clone(), base_src.to_string());
    files.insert(child_file.clone(), child_src.to_string());
    files.insert(impl_file.clone(), impl_src.to_string());

    let db = RefactorJavaDatabase::new(files.clone().into_iter());

    let offset = base_src.find("process").unwrap() + 1;
    let symbol = db.symbol_at(&base_file, offset).expect("I.process");

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
    let updated_impl = updated.get(&impl_file).unwrap();

    assert!(
        updated_base.contains("void handle();"),
        "expected I.process renamed:\n{updated_base}"
    );
    assert!(
        updated_impl.contains("void handle()"),
        "expected C.process renamed via J extends I:\n{updated_impl}"
    );
}

#[test]
fn rename_method_interface_override_chain_detects_collisions_in_implementations() {
    let interface_file = FileId::new("I.java");
    let impl_file = FileId::new("C.java");

    let interface_src = r#"interface I {
  void process();
}
"#;

    // C already defines `handle()`, so renaming `process()` -> `handle()` should conflict once the
    // interface implementation chain is included in the rename.
    let impl_src = r#"class C implements I {
  @Override public void process(){}
  void handle(){}
}
"#;

    let mut files = BTreeMap::new();
    files.insert(interface_file.clone(), interface_src.to_string());
    files.insert(impl_file.clone(), impl_src.to_string());

    let db = RefactorJavaDatabase::new(files.clone().into_iter());

    let offset = interface_src.find("process").unwrap() + 1;
    let symbol = db
        .symbol_at(&interface_file, offset)
        .expect("I.process symbol");

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
                if file == &impl_file && name == "handle"
        )),
        "expected NameCollision in C, got: {conflicts:?}"
    );
}

#[test]
fn rename_method_renames_inherited_implementation_for_interface() {
    let interface_file = FileId::new("I.java");
    let base_file = FileId::new("Base.java");
    let impl_file = FileId::new("C.java");
    let use_file = FileId::new("Use.java");

    let interface_src = r#"interface I {
  void process();
}
"#;

    let base_src = r#"class Base {
  public void process(){}
}
"#;

    // C inherits the implementation from Base.
    let impl_src = r#"class C extends Base implements I {
  void call() {
    process();
  }
}
"#;

    let use_src = r#"class Use {
  void m(C c, I i) {
    c.process();
    i.process();
  }
}
"#;

    let mut files = BTreeMap::new();
    files.insert(interface_file.clone(), interface_src.to_string());
    files.insert(base_file.clone(), base_src.to_string());
    files.insert(impl_file.clone(), impl_src.to_string());
    files.insert(use_file.clone(), use_src.to_string());

    let db = RefactorJavaDatabase::new(files.clone().into_iter());

    let offset = interface_src.find("process").unwrap() + 1;
    let symbol = db
        .symbol_at(&interface_file, offset)
        .expect("I.process symbol");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "handle".into(),
        },
    )
    .unwrap();

    let updated = apply_workspace_edit(&files, &edit).expect("workspace edit applies");

    let updated_interface = updated.get(&interface_file).unwrap();
    let updated_base = updated.get(&base_file).unwrap();
    let updated_impl = updated.get(&impl_file).unwrap();
    let updated_use = updated.get(&use_file).unwrap();

    assert!(
        updated_interface.contains("void handle();"),
        "expected interface method renamed:\n{updated_interface}"
    );
    assert!(
        updated_base.contains("void handle()"),
        "expected inherited implementation renamed in Base:\n{updated_base}"
    );
    assert!(
        updated_impl.contains("handle();"),
        "expected unqualified call updated:\n{updated_impl}"
    );
    assert!(
        updated_use.contains("c.handle();"),
        "expected class-typed call site updated:\n{updated_use}"
    );
    assert!(
        updated_use.contains("i.handle();"),
        "expected interface-typed call site updated:\n{updated_use}"
    );
}

#[test]
fn rename_method_interface_rename_includes_overridden_superclass_methods() {
    let interface_file = FileId::new("I.java");
    let base_file = FileId::new("Base.java");
    let impl_file = FileId::new("C.java");
    let use_file = FileId::new("Use.java");

    let interface_src = r#"interface I {
  void process();
}
"#;

    let base_src = r#"class Base {
  public void process(){}
}
"#;

    // C both implements the interface and overrides the superclass method.
    let impl_src = r#"class C extends Base implements I {
  @Override public void process(){}
}
"#;

    let use_src = r#"class Use {
  void m(I i, Base b, C c) {
    i.process();
    b.process();
    c.process();
  }
}
"#;

    let mut files = BTreeMap::new();
    files.insert(interface_file.clone(), interface_src.to_string());
    files.insert(base_file.clone(), base_src.to_string());
    files.insert(impl_file.clone(), impl_src.to_string());
    files.insert(use_file.clone(), use_src.to_string());

    let db = RefactorJavaDatabase::new(files.clone().into_iter());

    let offset = interface_src.find("process").unwrap() + 1;
    let symbol = db
        .symbol_at(&interface_file, offset)
        .expect("I.process symbol");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "handle".into(),
        },
    )
    .unwrap();

    let updated = apply_workspace_edit(&files, &edit).expect("workspace edit applies");

    let updated_interface = updated.get(&interface_file).unwrap();
    let updated_base = updated.get(&base_file).unwrap();
    let updated_impl = updated.get(&impl_file).unwrap();
    let updated_use = updated.get(&use_file).unwrap();

    assert!(
        updated_interface.contains("void handle();"),
        "expected interface method renamed:\n{updated_interface}"
    );
    assert!(
        updated_base.contains("void handle()"),
        "expected superclass method renamed:\n{updated_base}"
    );
    assert!(
        updated_impl.contains("void handle()"),
        "expected override method renamed:\n{updated_impl}"
    );
    assert!(
        updated_use.contains("i.handle();"),
        "expected interface-typed call site updated:\n{updated_use}"
    );
    assert!(
        updated_use.contains("b.handle();"),
        "expected superclass-typed call site updated:\n{updated_use}"
    );
    assert!(
        updated_use.contains("c.handle();"),
        "expected subclass-typed call site updated:\n{updated_use}"
    );
}

#[test]
fn rename_method_base_rename_updates_inherited_interface_contracts() {
    let interface_file = FileId::new("I.java");
    let base_file = FileId::new("Base.java");
    let impl_file = FileId::new("C.java");
    let use_file = FileId::new("Use.java");

    let interface_src = r#"interface I {
  void process();
}
"#;

    let base_src = r#"class Base {
  public void process(){}
}
"#;

    // C inherits the implementation from Base.
    let impl_src = r#"class C extends Base implements I {
}
"#;

    let use_src = r#"class Use {
  void m(I i, Base b, C c) {
    i.process();
    b.process();
    c.process();
  }
}
"#;

    let mut files = BTreeMap::new();
    files.insert(interface_file.clone(), interface_src.to_string());
    files.insert(base_file.clone(), base_src.to_string());
    files.insert(impl_file.clone(), impl_src.to_string());
    files.insert(use_file.clone(), use_src.to_string());

    let db = RefactorJavaDatabase::new(files.clone().into_iter());

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

    let updated_interface = updated.get(&interface_file).unwrap();
    let updated_base = updated.get(&base_file).unwrap();
    let updated_use = updated.get(&use_file).unwrap();

    assert!(
        updated_interface.contains("void handle();"),
        "expected interface method renamed to preserve contract:\n{updated_interface}"
    );
    assert!(
        updated_base.contains("void handle()"),
        "expected base method renamed:\n{updated_base}"
    );
    assert!(
        updated_use.contains("i.handle();"),
        "expected interface-typed call site updated:\n{updated_use}"
    );
    assert!(
        updated_use.contains("b.handle();"),
        "expected base-typed call site updated:\n{updated_use}"
    );
    assert!(
        updated_use.contains("c.handle();"),
        "expected subclass-typed call site updated:\n{updated_use}"
    );
}
