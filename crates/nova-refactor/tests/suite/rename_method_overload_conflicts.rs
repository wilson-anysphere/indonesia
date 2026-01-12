use nova_refactor::{
    apply_text_edits, rename, Conflict, FileId, RefactorJavaDatabase, RenameParams,
    SemanticRefactorError,
};

#[test]
fn rename_method_allows_expanding_overload_set() {
    let file = FileId::new("Test.java");
    let src = r#"class C {
  void foo(int x) {}
  void bar(String s) {}
  void m() { foo(1); }
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("void foo").unwrap() + "void ".len() + 1;
    let symbol = db.symbol_at(&file, offset).expect("symbol at foo");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "bar".into(),
        },
    )
    .unwrap();

    let after = apply_text_edits(src, &edit.text_edits).unwrap();
    assert!(
        after.contains("void bar(int x)"),
        "expected foo(int) to be renamed to bar(int): {after}"
    );
    assert!(
        after.contains("void bar(String s)"),
        "expected existing bar(String) overload to remain: {after}"
    );
    assert!(
        after.contains("void m() { bar(1); }"),
        "expected call site to be renamed: {after}"
    );
    assert!(!after.contains("foo("), "expected no remaining foo() calls: {after}");
}

#[test]
fn rename_method_rejects_duplicate_signature_in_same_type() {
    let file = FileId::new("Test.java");
    let src = r#"class C {
  void foo(int x) {}
  void bar(int y) {}
}
"#;

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("void foo").unwrap() + "void ".len() + 1;
    let symbol = db.symbol_at(&file, offset).expect("symbol at foo");

    let err = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "bar".into(),
        },
    )
    .unwrap_err();

    let SemanticRefactorError::Conflicts(conflicts) = err else {
        panic!("expected conflicts, got: {err:?}");
    };

    assert!(
        conflicts
            .iter()
            .any(|c| matches!(c, Conflict::NameCollision { name, .. } if name == "bar")),
        "expected NameCollision conflict: {conflicts:?}"
    );
}
