use crate::text_fixture::offset_to_position;
use lsp_types::{CodeActionKind, CodeActionOrCommand, DiagnosticSeverity, NumberOrString, Range};
use nova_config::NovaConfig;
use nova_db::InMemoryFileStore;
use nova_ext::{ProjectId, Span};
use nova_ide::{
    code_action::diagnostic_quick_fixes, extensions::IdeExtensions, file_diagnostics,
    file_diagnostics_lsp,
};
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
fn diagnostics_treats_fully_qualified_reference_as_unused_import() {
    let mut db = InMemoryFileStore::new();
    let path = PathBuf::from("/test.java");
    let file = db.file_id_for_path(&path);
    db.set_file_text(
        file,
        r#"import java.util.List;
class A {
  java.util.List<String> xs;
}
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
fn diagnostics_do_not_count_comment_mentions_as_usage() {
    let mut db = InMemoryFileStore::new();
    let path = PathBuf::from("/test.java");
    let file = db.file_id_for_path(&path);
    db.set_file_text(
        file,
        r#"import java.util.List;
// List is only mentioned in a comment.
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
fn diagnostics_do_not_treat_block_comment_imports_as_real_imports() {
    let mut db = InMemoryFileStore::new();
    let path = PathBuf::from("/test.java");
    let file = db.file_id_for_path(&path);
    db.set_file_text(
        file,
        r#"/*
import java.util.List;
*/
class A {}
"#
        .to_string(),
    );

    let diags = file_diagnostics(&db, file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unused-import"),
        "expected no unused-import diagnostics; got {diags:#?}"
    );
}

#[test]
fn diagnostics_do_not_consume_the_whole_file_when_an_import_is_missing_a_semicolon() {
    let mut db = InMemoryFileStore::new();
    let path = PathBuf::from("/test.java");
    let file = db.file_id_for_path(&path);
    db.set_file_text(
        file,
        "import java.util.List\nimport java.util.Map;\nclass A { Map<String, String> m; }\n"
            .to_string(),
    );

    let diags = file_diagnostics(&db, file);
    assert!(
        diags.iter().all(|d| d.code.as_ref() != "unused-import"),
        "expected no unused-import diagnostics; got {diags:#?}"
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

#[test]
fn diagnostic_quick_fixes_includes_remove_unused_import() {
    let mut db = InMemoryFileStore::new();
    let path = PathBuf::from("/test.java");
    let file = db.file_id_for_path(&path);
    let source = "import java.util.List;\nclass A {}\n";
    db.set_file_text(file, source.to_string());

    let lsp_diags = file_diagnostics_lsp(&db, file);
    let diag = lsp_diags
        .iter()
        .find(|diag| matches!(&diag.code, Some(NumberOrString::String(code)) if code == "unused-import"))
        .expect("expected unused-import diagnostic");

    let abs = nova_core::AbsPathBuf::new(path).expect("absolute path");
    let uri: lsp_types::Uri = nova_core::path_to_file_uri(&abs)
        .expect("file uri")
        .parse()
        .expect("parse uri");

    let actions = diagnostic_quick_fixes(source, Some(uri.clone()), diag.range.clone(), &lsp_diags);
    let action = actions
        .iter()
        .find(|action| action.title == "Remove unused import")
        .expect("expected Remove unused import code action");

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
fn code_actions_with_context_includes_remove_unused_import_for_cursor_at_span_start() {
    let mut db = InMemoryFileStore::new();
    let path = PathBuf::from("/test.java");
    let file = db.file_id_for_path(&path);
    let source = "import java.util.List;\nclass A {}\n";
    db.set_file_text(file, source.to_string());

    let import_start = source.find("import").expect("expected import in fixture");
    let import_end = source.find('\n').expect("expected newline after import");

    let range = Range::new(
        offset_to_position(source, import_start),
        offset_to_position(source, import_end),
    );

    let diag = lsp_types::Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::WARNING),
        code: Some(NumberOrString::String("unused-import".to_string())),
        message: "unused import".to_string(),
        ..lsp_types::Diagnostic::default()
    };

    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(db);
    let ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), ProjectId::new(0));

    let selection = Span::new(import_start, import_start);
    let actions =
        ide.code_actions_lsp_with_context(CancellationToken::new(), file, Some(selection), &[diag]);

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
}
