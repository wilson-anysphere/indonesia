use std::path::PathBuf;
use std::sync::Arc;

use lsp_types::{CodeActionOrCommand, Position};
use nova_config::NovaConfig;
use nova_db::{InMemoryFileStore, SalsaDbView};
use nova_ext::{ProjectId, Span};
use nova_ide::extensions::IdeExtensions;
use nova_scheduler::CancellationToken;

use crate::text_fixture::position_to_offset;

fn apply_lsp_edits(text: &str, edits: &[lsp_types::TextEdit]) -> String {
    use nova_core::{LineIndex, Position as CorePosition};

    let index = LineIndex::new(text);
    let mut core_edits = Vec::new();
    for edit in edits {
        let range = nova_core::Range::new(
            CorePosition::new(edit.range.start.line, edit.range.start.character),
            CorePosition::new(edit.range.end.line, edit.range.end.character),
        );
        let range = index.text_range(text, range).expect("valid LSP range");
        core_edits.push(nova_core::TextEdit::new(range, edit.new_text.clone()));
    }

    nova_core::apply_text_edits(text, &core_edits).expect("apply edits")
}

fn apply_single_text_edit(text: &str, edit: &lsp_types::TextEdit) -> String {
    apply_lsp_edits(text, std::slice::from_ref(edit))
}

fn apply_workspace_edit(text: &str, edit: &lsp_types::WorkspaceEdit) -> String {
    let changes = edit
        .changes
        .as_ref()
        .expect("expected WorkspaceEdit.changes (not document_changes)");
    assert_eq!(
        changes.len(),
        1,
        "expected a single-file WorkspaceEdit in quick-fix tests"
    );
    let (_uri, edits) = changes.iter().next().expect("expected file edits");
    apply_lsp_edits(text, edits)
}

fn find_code_action<'a>(
    actions: &'a [lsp_types::CodeActionOrCommand],
    title: &str,
) -> Option<&'a lsp_types::CodeAction> {
    actions.iter().find_map(|action| match action {
        lsp_types::CodeActionOrCommand::CodeAction(action) if action.title == title => Some(action),
        _ => None,
    })
}

fn first_text_edit(action: &lsp_types::CodeAction) -> &lsp_types::TextEdit {
    let edit = action.edit.as_ref().expect("expected workspace edit");
    let changes = edit
        .changes
        .as_ref()
        .expect("expected WorkspaceEdit.changes (not document_changes)");
    let (_uri, edits) = changes.iter().next().expect("expected at least one file edit");
    edits.first().expect("expected at least one TextEdit")
}

fn action_titles(actions: &[CodeActionOrCommand]) -> Vec<&str> {
    actions
        .iter()
        .filter_map(|action| match action {
            CodeActionOrCommand::CodeAction(action) => Some(action.title.as_str()),
            CodeActionOrCommand::Command(command) => Some(command.title.as_str()),
        })
        .collect()
}

#[test]
fn unresolved_name_offers_create_variable_and_field_quick_fixes() {
    let source = "class A {\n  void m() {\n    int x = y;\n  }\n}\n";

    let mut db = InMemoryFileStore::new();
    let path = PathBuf::from("/test.java");
    let file = db.file_id_for_path(&path);
    db.set_file_text(file, source.to_string());

    // `IdeExtensions` requires a `Send + Sync` database; wrap our in-memory store in a
    // snapshot-like view.
    let view = SalsaDbView::from_source_db(&db);
    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(view);
    let ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), ProjectId::new(0));

    let y_offset = source.find("y;").expect("expected `y` in fixture");
    let y_span = Span::new(y_offset, y_offset + 1);

    let actions = ide.code_actions_lsp(CancellationToken::new(), file, Some(y_span));
    let local = find_code_action(&actions, "Create local variable 'y'").unwrap_or_else(|| {
        panic!(
            "missing local-variable quick fix; got titles {:?}",
            action_titles(&actions)
        )
    });
    let field = find_code_action(&actions, "Create field 'y'").unwrap_or_else(|| {
        panic!(
            "missing field quick fix; got titles {:?}",
            action_titles(&actions)
        )
    });
    // Local variable: inserted on the line before `int x = y;` (i.e. at the start of that line).
    let updated = apply_lsp_edits(source, std::slice::from_ref(first_text_edit(local)));
    assert!(
        updated.contains("    Object y = null;\n    int x = y;"),
        "expected local-variable stub before statement; got:\n{updated}"
    );
    assert_eq!(first_text_edit(local).range.start, Position::new(2, 0));

    // Field: inserted near the end of the class (before final `}`).
    let updated = apply_lsp_edits(source, std::slice::from_ref(first_text_edit(field)));
    assert!(
        updated.contains("  private Object y;\n}"),
        "expected field stub before final brace; got:\n{updated}"
    );
    assert_eq!(first_text_edit(field).range.start, Position::new(4, 0));
}

#[test]
fn void_method_return_value_offers_remove_returned_value_quickfix() {
    let source = r#"class A { void m() { return 1; } }"#.to_string();

    let mut db = InMemoryFileStore::new();
    let path = PathBuf::from("/test.java");
    let file = db.file_id_for_path(&path);
    db.set_file_text(file, source.clone());

    let view = SalsaDbView::from_source_db(&db);
    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(view);
    let ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), ProjectId::new(0));

    let expr_start = source
        .find("return 1")
        .expect("expected return statement")
        + "return ".len();
    let expr_span = Span::new(expr_start, expr_start + 1);

    let actions = ide.code_actions_lsp(CancellationToken::new(), file, Some(expr_span));
    let action =
        find_code_action(&actions, "Remove returned value").expect("missing quick fix action");
    assert_eq!(
        action.kind.as_ref(),
        Some(&lsp_types::CodeActionKind::QUICKFIX)
    );

    let updated = apply_lsp_edits(&source, std::slice::from_ref(first_text_edit(action)));
    assert!(
        updated.contains("return ;"),
        "expected returned value to be removed; got {updated:?}"
    );

    // Ensure the quick fix is only offered when the selection intersects the diagnostic span.
    let return_start = source.find("return").expect("return keyword");
    let return_span = Span::new(return_start, return_start + "return".len());
    let actions = ide.code_actions_lsp(CancellationToken::new(), file, Some(return_span));
    assert!(
        find_code_action(&actions, "Remove returned value").is_none(),
        "quick fix should not be offered when selection is outside diagnostic span"
    );
}

#[test]
fn return_type_mismatch_offers_cast_quickfix() {
    let source = r#"class A { String m() { Object o = ""; return o; } }"#.to_string();

    let mut db = InMemoryFileStore::new();
    let path = PathBuf::from("/test.java");
    let file = db.file_id_for_path(&path);
    db.set_file_text(file, source.clone());

    let view = SalsaDbView::from_source_db(&db);
    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(view);
    let ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), ProjectId::new(0));

    let expr_start = source
        .find("return o")
        .expect("expected return statement")
        + "return ".len();
    let expr_span = Span::new(expr_start, expr_start + 1);

    let actions = ide.code_actions_lsp(CancellationToken::new(), file, Some(expr_span));
    let action = find_code_action(&actions, "Cast to String").expect("missing cast quick fix");
    let updated = apply_lsp_edits(&source, std::slice::from_ref(first_text_edit(action)));
    assert!(
        updated.contains("return (String) (o);"),
        "expected return expression to be cast; got {updated:?}"
    );

    // Ensure the quick fix is only offered when the selection intersects the diagnostic span.
    let decl_start = source
        .find("Object o")
        .expect("expected variable declaration")
        + "Object ".len();
    let decl_span = Span::new(decl_start, decl_start + 1);
    let actions = ide.code_actions_lsp(CancellationToken::new(), file, Some(decl_span));
    assert!(
        find_code_action(&actions, "Cast to String").is_none(),
        "quick fix should not be offered when selection is outside diagnostic span"
    );
}

#[test]
fn unresolved_type_offers_create_class_quick_fix() {
    let source = "class A { MissingType x; }";

    let mut db = InMemoryFileStore::new();
    let path = PathBuf::from("/test.java");
    let file = db.file_id_for_path(&path);
    db.set_file_text(file, source.to_string());

    // `IdeExtensions` requires a `Send + Sync` database; wrap our in-memory store in a
    // snapshot-like view.
    let view = SalsaDbView::from_source_db(&db);
    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(view);
    let ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), ProjectId::new(0));

    let missing_start = source
        .find("MissingType")
        .expect("expected MissingType in fixture");
    let missing_end = missing_start + "MissingType".len();
    let missing_span = Span::new(missing_start, missing_end);

    let actions = ide.code_actions_lsp(CancellationToken::new(), file, Some(missing_span));
    let action = actions
        .iter()
        .find_map(|action| match action {
            CodeActionOrCommand::CodeAction(action)
                if action.title == "Create class 'MissingType'" =>
            {
                Some(action)
            }
            _ => None,
        })
        .unwrap_or_else(|| {
            let titles: Vec<_> = actions
                .iter()
                .filter_map(|a| match a {
                    CodeActionOrCommand::CodeAction(a) => Some(a.title.as_str()),
                    CodeActionOrCommand::Command(c) => Some(c.title.as_str()),
                })
                .collect();
            panic!("missing Create class quick fix; got titles {titles:?}");
        });

    let edit = action
        .edit
        .as_ref()
        .expect("create-class quick fix should have edit");
    let Some(changes) = edit.changes.as_ref() else {
        panic!("expected WorkspaceEdit.changes");
    };
    let (_, edits) = changes.iter().next().expect("expected at least one edit");
    assert_eq!(edits.len(), 1, "expected exactly one TextEdit");

    let edit = &edits[0];
    let offset = position_to_offset(source, edit.range.start).expect("start offset");
    assert_eq!(
        offset,
        source.len(),
        "expected create-class edit to insert at EOF"
    );

    let updated = apply_single_text_edit(source, edit);
    assert_eq!(
        updated, "class A { MissingType x; }\n\nclass MissingType {\n}\n",
        "unexpected updated text:\n{updated}"
    );
}

#[test]
fn create_field_quick_fix_in_single_line_file_inserts_before_final_brace() {
    let source = "class A { void m() { int x = y; } }";

    let mut db = InMemoryFileStore::new();
    let path = PathBuf::from("/test.java");
    let file = db.file_id_for_path(&path);
    db.set_file_text(file, source.to_string());

    // `IdeExtensions` requires a `Send + Sync` database; wrap our in-memory store in a
    // snapshot-like view.
    let view = SalsaDbView::from_source_db(&db);
    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(view);
    let ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), ProjectId::new(0));

    let y_offset = source.find("y;").expect("expected `y` in fixture");
    let y_span = Span::new(y_offset, y_offset + 1);

    let actions = ide.code_actions_lsp(CancellationToken::new(), file, Some(y_span));
    let field = actions
        .iter()
        .filter_map(|action| match action {
            CodeActionOrCommand::CodeAction(action) => Some(action),
            CodeActionOrCommand::Command(_) => None,
        })
        .find(|action| action.title == "Create field 'y'")
        .expect("expected Create field quick fix");

    let field_edit = field
        .edit
        .as_ref()
        .expect("field quick fix should have edit");
    let updated = apply_workspace_edit(source, field_edit);

    assert!(
        updated.starts_with("class A"),
        "expected file to still start with class declaration; got:\n{updated}"
    );
    assert!(
        updated.contains("private Object y;"),
        "expected inserted field; got:\n{updated}"
    );
    assert!(
        updated.ends_with("\n}"),
        "expected inserted field to end with closing brace on its own line; got:\n{updated}"
    );
}
