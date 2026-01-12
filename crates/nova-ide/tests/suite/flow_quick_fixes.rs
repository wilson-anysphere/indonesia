use lsp_types::{CodeActionKind, Position, Range, TextEdit};
use nova_db::InMemoryFileStore;
use nova_ide::{code_action::diagnostic_quick_fixes, file_diagnostics, file_diagnostics_lsp};
use nova_types::{Severity, Span};
use std::path::PathBuf;

use crate::text_fixture::{offset_to_position as offset_to_lsp_position, position_to_offset};

fn fixture_file(text: &str) -> (InMemoryFileStore, nova_db::FileId) {
    let mut db = InMemoryFileStore::new();
    let path = PathBuf::from("/test.java");
    let file = db.file_id_for_path(&path);
    db.set_file_text(file, text.to_string());
    (db, file)
}

fn lsp_position_to_offset(text: &str, pos: lsp_types::Position) -> usize {
    position_to_offset(text, pos).unwrap_or(text.len())
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

    // Apply from back to front.
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
        diags.iter().any(|d| {
            d.code == "FLOW_UNREACHABLE"
                && d.severity == Severity::Warning
                && d.span
                    .is_some_and(|span| span.start < stmt_end && span.end > stmt_start)
        }),
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

    let edit = action
        .edit
        .clone()
        .expect("expected quickfix workspace edit");
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

#[test]
fn flow_unassigned_quick_fix_initializes_variable_with_primitive_default() {
    let text = r#"class A {
  void m() {
    int x;
    System.out.println(x);
  }
}
"#;

    let (db, file) = fixture_file(text);

    let x_use_offset = text
        .find("System.out.println(x)")
        .expect("fixture must contain println(x)")
        + "System.out.println(".len();
    let x_span = Span::new(x_use_offset, x_use_offset + 1);

    let diags = file_diagnostics(&db, file);
    let diag = diags
        .iter()
        .find(|d| d.code.as_ref() == "FLOW_UNASSIGNED")
        .expect("expected FLOW_UNASSIGNED diagnostic");
    assert_eq!(diag.severity, Severity::Error);
    assert_eq!(diag.span, Some(x_span));

    let lsp_diags = file_diagnostics_lsp(&db, file);

    let selection = Range::new(
        offset_to_lsp_position(text, x_span.start),
        offset_to_lsp_position(text, x_span.end),
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
        .find(|action| action.title == "Initialize 'x'")
        .expect("expected Initialize 'x' quickfix");
    assert_eq!(action.kind, Some(CodeActionKind::QUICKFIX));

    let edit = action
        .edit
        .clone()
        .expect("expected quickfix workspace edit");
    let changes = edit.changes.expect("expected `changes` workspace edit");
    let edits = changes.get(&uri).expect("expected edits for file uri");
    assert_eq!(edits.len(), 1, "expected single text edit; got {edits:#?}");

    let edit = &edits[0];
    assert_eq!(edit.new_text, "    x = 0;\n");
    assert_eq!(edit.range.start, Position::new(3, 0));
    assert_eq!(edit.range.end, edit.range.start);

    let updated = apply_lsp_text_edits(text, edits);
    assert!(
        updated.contains("    x = 0;\n    System.out.println(x);"),
        "expected initialization to be inserted before first use; updated text:\n{updated}"
    );
}
