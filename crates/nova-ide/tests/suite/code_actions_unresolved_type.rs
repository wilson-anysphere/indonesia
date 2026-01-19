use nova_config::NovaConfig;
use nova_db::InMemoryFileStore;
use nova_ide::extensions::IdeExtensions;
use nova_scheduler::CancellationToken;
use nova_test_utils::apply_lsp_edits;
use nova_types::Span;
use std::path::PathBuf;
use std::sync::Arc;

#[test]
fn unresolved_type_quickfix_offers_use_fully_qualified_name() {
    let source = r#"class A {
  void m(List<String> xs) {}
}
"#;

    let mut db = InMemoryFileStore::new();
    let file = db.file_id_for_path(PathBuf::from("/test.java"));
    db.set_file_text(file, source.to_string());

    let list_start = source.find("List<String>").expect("List occurrence");
    let list_end = list_start + "List".len();
    let selection = Span::new(list_start, list_end);

    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(db);
    let ide = IdeExtensions::new(
        db,
        Arc::new(NovaConfig::default()),
        nova_ext::ProjectId::new(0),
    );

    let actions = ide.code_actions_lsp(CancellationToken::new(), file, Some(selection));

    let action = actions
        .iter()
        .filter_map(|action| match action {
            lsp_types::CodeActionOrCommand::CodeAction(action) => Some(action),
            _ => None,
        })
        .find(|action| action.title == "Use fully qualified name 'java.util.List'")
        .expect("expected fully-qualified-name quickfix");

    assert_eq!(action.kind, Some(lsp_types::CodeActionKind::QUICKFIX));

    let edit = action.edit.as_ref().expect("expected workspace edit");
    let changes = edit.changes.as_ref().expect("expected changes map");
    let edits = changes.values().next().expect("expected edits");
    let updated = apply_lsp_edits(source, edits);

    assert!(
        updated.contains("void m(java.util.List<String> xs) {}"),
        "expected updated source to use fully qualified name; got:\n{updated}"
    );
}

#[test]
fn unresolved_type_quickfix_offers_use_fully_qualified_name_for_cursor_selection() {
    let source = r#"class A {
  void m(List<String> xs) {}
}
"#;

    let mut db = InMemoryFileStore::new();
    let file = db.file_id_for_path(PathBuf::from("/test.java"));
    db.set_file_text(file, source.to_string());

    let list_start = source.find("List<String>").expect("List occurrence");
    // Cursor selection (zero-length span) at the start of `List`.
    let selection = Span::new(list_start, list_start);

    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(db);
    let ide = IdeExtensions::new(
        db,
        Arc::new(NovaConfig::default()),
        nova_ext::ProjectId::new(0),
    );

    let actions = ide.code_actions_lsp(CancellationToken::new(), file, Some(selection));

    let action = actions
        .iter()
        .filter_map(|action| match action {
            lsp_types::CodeActionOrCommand::CodeAction(action) => Some(action),
            _ => None,
        })
        .find(|action| action.title == "Use fully qualified name 'java.util.List'")
        .expect("expected fully-qualified-name quickfix");

    let edit = action.edit.as_ref().expect("expected workspace edit");
    let changes = edit.changes.as_ref().expect("expected changes map");
    let edits = changes.values().next().expect("expected edits");
    let updated = apply_lsp_edits(source, edits);

    assert!(
        updated.contains("void m(java.util.List<String> xs) {}"),
        "expected updated source to use fully qualified name; got:\n{updated}"
    );
}

#[test]
fn unresolved_type_quickfix_offers_use_fully_qualified_name_for_cursor_at_span_end() {
    let source = r#"class A {
  void m(List<String> xs) {}
}
"#;

    let mut db = InMemoryFileStore::new();
    let file = db.file_id_for_path(PathBuf::from("/test.java"));
    db.set_file_text(file, source.to_string());

    let list_start = source.find("List<String>").expect("List occurrence");
    let list_end = list_start + "List".len();
    // Cursor selection (zero-length span) at the end of `List`.
    let selection = Span::new(list_end, list_end);

    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(db);
    let ide = IdeExtensions::new(
        db,
        Arc::new(NovaConfig::default()),
        nova_ext::ProjectId::new(0),
    );

    let actions = ide.code_actions_lsp(CancellationToken::new(), file, Some(selection));

    let action = actions
        .iter()
        .filter_map(|action| match action {
            lsp_types::CodeActionOrCommand::CodeAction(action) => Some(action),
            _ => None,
        })
        .find(|action| action.title == "Use fully qualified name 'java.util.List'")
        .expect("expected fully-qualified-name quickfix");

    let edit = action.edit.as_ref().expect("expected workspace edit");
    let changes = edit.changes.as_ref().expect("expected changes map");
    let edits = changes.values().next().expect("expected edits");
    let updated = apply_lsp_edits(source, edits);

    assert!(
        updated.contains("void m(java.util.List<String> xs) {}"),
        "expected updated source to use fully qualified name; got:\n{updated}"
    );
}
