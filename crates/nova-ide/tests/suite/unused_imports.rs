use lsp_types::{CodeActionKind, CodeActionOrCommand};
use nova_config::NovaConfig;
use nova_db::InMemoryFileStore;
use nova_ext::{ProjectId, Span};
use nova_ide::{extensions::IdeExtensions, file_diagnostics};
use nova_refactor::position_to_offset_utf16;
use nova_scheduler::CancellationToken;
use nova_types::Severity;
use std::path::PathBuf;
use std::sync::Arc;

#[test]
fn diagnostics_include_unused_import() {
    let mut db = InMemoryFileStore::new();
    let path = PathBuf::from("/test.java");
    let file = db.file_id_for_path(&path);
    db.set_file_text(
        file,
        r#"import java.util.List;
class A {}
"#
        .to_string(),
    );

    let diags = file_diagnostics(&db, file);
    assert!(
        diags
            .iter()
            .any(|d| d.severity == Severity::Warning && d.code.as_ref() == "unused-import"),
        "expected unused-import warning diagnostic; got {diags:#?}"
    );
}

#[test]
fn quick_fix_removes_unused_import_line() {
    let mut db = InMemoryFileStore::new();
    let path = PathBuf::from("/test.java");
    let file = db.file_id_for_path(&path);
    let source = "import java.util.List;\nclass A {}\n";
    db.set_file_text(file, source.to_string());

    let selection_offset = source
        .find("java.util.List")
        .expect("expected import in fixture");
    let selection = Span::new(selection_offset, selection_offset + 1);

    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(db);
    let ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), ProjectId::new(0));
    let actions = ide.code_actions_lsp(CancellationToken::new(), file, Some(selection));

    let action = actions
        .iter()
        .find_map(|action| match action {
            CodeActionOrCommand::CodeAction(action) if action.title == "Remove unused import" => {
                Some(action)
            }
            _ => None,
        })
        .expect("expected Remove unused import code action");

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
