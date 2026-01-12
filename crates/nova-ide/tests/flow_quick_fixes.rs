use lsp_types::{CodeActionKind, Position, Range, TextEdit};
use nova_db::InMemoryFileStore;
use nova_ide::{code_action::diagnostic_quick_fixes, file_diagnostics, file_diagnostics_lsp};
use nova_types::Severity;
use std::path::PathBuf;

fn fixture_file(text: &str) -> (InMemoryFileStore, nova_db::FileId) {
    let mut db = InMemoryFileStore::new();
    let path = PathBuf::from("/test.java");
    let file = db.file_id_for_path(&path);
    db.set_file_text(file, text.to_string());
    (db, file)
}

fn lsp_position_to_offset(text: &str, pos: lsp_types::Position) -> usize {
    let mut line = 0u32;
    let mut col_utf16 = 0u32;
    let mut offset = 0usize;

    for ch in text.chars() {
        if line == pos.line && col_utf16 == pos.character {
            return offset;
        }
        offset += ch.len_utf8();
        if ch == '\n' {
            line += 1;
            col_utf16 = 0;
        } else {
            col_utf16 += ch.len_utf16() as u32;
        }
    }

    if line == pos.line && col_utf16 == pos.character {
        offset
    } else {
        text.len()
    }
}

fn offset_to_lsp_position(text: &str, offset: usize) -> Position {
    let mut line = 0u32;
    let mut col_utf16 = 0u32;
    let mut cur = 0usize;

    for ch in text.chars() {
        if cur >= offset {
            break;
        }
        cur += ch.len_utf8();
        if ch == '\n' {
            line += 1;
            col_utf16 = 0;
        } else {
            col_utf16 += ch.len_utf16() as u32;
        }
    }

    Position::new(line, col_utf16)
}

fn apply_lsp_text_edits(text: &str, edits: &[TextEdit]) -> String {
    let mut edits_with_offsets: Vec<(usize, usize, String)> = edits
        .iter()
        .map(|edit| {
            let start = lsp_position_to_offset(text, edit.range.start);
            let end = lsp_position_to_offset(text, edit.range.end);
            (start, end, edit.new_text.clone())
        })
        .collect();

    edits_with_offsets.sort_by(|(a_start, _, _), (b_start, _, _)| b_start.cmp(a_start));

    let mut out = text.to_string();
    for (start, end, new_text) in edits_with_offsets {
        out.replace_range(start..end, &new_text);
    }
    out
}

#[test]
fn unreachable_code_quick_fix_removes_statement() {
    let text = r#"class A {
  void m() {
    return;
    int x = 1;
  }
}
"#;

    let (db, file) = fixture_file(text);

    let stmt = "int x = 1;";
    let stmt_start = text.find(stmt).expect("expected unreachable statement");
    let stmt_end = stmt_start + stmt.len();

    let diags = file_diagnostics(&db, file);
    assert!(
        diags.iter().any(|d| d.code == "FLOW_UNREACHABLE"
            && d.severity == Severity::Warning
            && d.span
                .is_some_and(|span| span.start < stmt_end && span.end > stmt_start)),
        "expected FLOW_UNREACHABLE diagnostic on `{stmt}`; got {diags:#?}"
    );

    let lsp_diags = file_diagnostics_lsp(&db, file);

    let selection = Range::new(
        offset_to_lsp_position(text, stmt_start),
        offset_to_lsp_position(text, stmt_end),
    );

    let path = PathBuf::from("/test.java");
    let abs = nova_core::AbsPathBuf::new(path).expect("absolute path");
    let uri: lsp_types::Uri = nova_core::path_to_file_uri(&abs)
        .expect("file uri")
        .parse()
        .expect("parse uri");

    let actions = diagnostic_quick_fixes(text, Some(uri.clone()), selection, &lsp_diags);

    let action = actions
        .iter()
        .find(|action| action.title == "Remove unreachable code")
        .expect("expected `Remove unreachable code` quickfix");

    assert_eq!(action.kind, Some(CodeActionKind::QUICKFIX));

    let edit = action.edit.clone().expect("expected quickfix workspace edit");
    let changes = edit.changes.expect("expected `changes` workspace edit");

    let edits = changes.get(&uri).expect("expected edits for file uri");
    assert_eq!(edits.len(), 1, "expected single text edit; got {edits:#?}");

    let edit = &edits[0];
    assert!(
        edit.new_text.is_empty(),
        "expected removal edit; got new_text={:?}",
        edit.new_text
    );

    let start = lsp_position_to_offset(text, edit.range.start);
    let end = lsp_position_to_offset(text, edit.range.end);
    assert!(
        text[start..end].contains(stmt),
        "expected edit range to cover `{stmt}`; range covered {:?}",
        &text[start..end]
    );

    let updated = apply_lsp_text_edits(text, edits);
    assert!(
        !updated.contains(stmt),
        "expected statement to be removed; updated text:\n{updated}"
    );

    // Filtering: requesting code actions on a non-overlapping span should not surface the quick fix.
    let return_stmt = "return;";
    let return_start = text.find(return_stmt).expect("expected return statement");
    let return_end = return_start + return_stmt.len();
    let selection = Range::new(
        offset_to_lsp_position(text, return_start),
        offset_to_lsp_position(text, return_end),
    );
    let actions = diagnostic_quick_fixes(text, Some(uri), selection, &lsp_diags);
    assert!(
        !actions
            .iter()
            .any(|action| action.title == "Remove unreachable code"),
        "expected quick fix to be filtered out for non-intersecting selection; got {actions:#?}"
    );
}
