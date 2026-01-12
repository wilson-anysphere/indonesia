use lsp_types::{CodeActionKind, CodeActionOrCommand, NumberOrString};
use nova_config::NovaConfig;
use nova_db::InMemoryFileStore;
use nova_ext::{ProjectId, Span};
use nova_ide::{code_action::diagnostic_quick_fixes, extensions::IdeExtensions, file_diagnostics_lsp};
use nova_refactor::position_to_offset_utf16;
use nova_scheduler::CancellationToken;
use std::path::PathBuf;
use std::sync::Arc;

#[test]
fn diagnostic_quick_fixes_includes_remove_unresolved_import() {
    let mut db = InMemoryFileStore::new();
    let path = PathBuf::from("/test.java");
    let file = db.file_id_for_path(&path);
    let source = "import foo.Bar;\nclass A {}\n";
    db.set_file_text(file, source.to_string());

    let lsp_diags = file_diagnostics_lsp(&db, file);
    let diag = lsp_diags
        .iter()
        .find(|diag| {
            matches!(&diag.code, Some(NumberOrString::String(code)) if code == "unresolved-import")
        })
        .expect("expected unresolved-import diagnostic");

    let abs = nova_core::AbsPathBuf::new(path).expect("absolute path");
    let uri: lsp_types::Uri = nova_core::path_to_file_uri(&abs)
        .expect("file uri")
        .parse()
        .expect("parse uri");

    let actions = diagnostic_quick_fixes(source, Some(uri.clone()), diag.range.clone(), &lsp_diags);
    let action = actions
        .iter()
        .find(|action| action.title == "Remove unresolved import")
        .expect("expected Remove unresolved import code action");

    assert_eq!(action.kind, Some(CodeActionKind::QUICKFIX));
    let edit = action.edit.as_ref().expect("expected workspace edit");
    let changes = edit.changes.as_ref().expect("expected changes-based edit");
    let edits = changes.get(&uri).expect("expected edits for file uri");
    assert_eq!(edits.len(), 1);
    let text_edit = &edits[0];
    assert!(text_edit.new_text.is_empty());
    assert_eq!(text_edit.range.start.line, 0);
    assert_eq!(text_edit.range.start.character, 0);
    assert_eq!(text_edit.range.end.line, 1);
    assert_eq!(text_edit.range.end.character, 0);

    let start = position_to_offset_utf16(source, text_edit.range.start).expect("start offset");
    let end = position_to_offset_utf16(source, text_edit.range.end).expect("end offset");
    let mut updated = source.to_string();
    updated.replace_range(start..end, &text_edit.new_text);
    assert_eq!(updated, "class A {}\n");
}

#[test]
fn code_actions_lsp_offers_remove_unresolved_import_quick_fix() {
    let mut db = InMemoryFileStore::new();
    let path = PathBuf::from("/test.java");
    let file = db.file_id_for_path(&path);
    let source = "import foo.Bar;\nclass A {}\n";
    db.set_file_text(file, source.to_string());

    let cursor_offset = source.find("foo.Bar").expect("expected import in fixture");
    let selection = Span::new(cursor_offset, cursor_offset);

    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(db);
    let ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), ProjectId::new(0));
    let actions = ide.code_actions_lsp(CancellationToken::new(), file, Some(selection));

    let action = actions
        .iter()
        .find_map(|action| match action {
            CodeActionOrCommand::CodeAction(action)
                if action.title == "Remove unresolved import" =>
            {
                Some(action)
            }
            _ => None,
        })
        .expect("expected Remove unresolved import code action");

    assert_eq!(action.kind, Some(CodeActionKind::QUICKFIX));
    let edit = action.edit.as_ref().expect("expected workspace edit");
    let changes = edit.changes.as_ref().expect("expected changes-based edit");
    let edits = changes.values().next().expect("expected text edits");
    assert_eq!(edits.len(), 1);
    let text_edit = &edits[0];
    assert!(text_edit.new_text.is_empty());
    assert_eq!(text_edit.range.start.line, 0);
    assert_eq!(text_edit.range.start.character, 0);
    assert_eq!(text_edit.range.end.line, 1);
    assert_eq!(text_edit.range.end.character, 0);

    let start = position_to_offset_utf16(source, text_edit.range.start).expect("start offset");
    let end = position_to_offset_utf16(source, text_edit.range.end).expect("end offset");
    let mut updated = source.to_string();
    updated.replace_range(start..end, &text_edit.new_text);
    assert_eq!(updated, "class A {}\n");
}
