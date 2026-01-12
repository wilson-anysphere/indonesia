use nova_config::NovaConfig;
use nova_db::InMemoryFileStore;
use nova_ext::{ProjectId, Span};
use nova_ide::extensions::IdeExtensions;
use nova_scheduler::CancellationToken;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

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

fn first_edit(action: &lsp_types::CodeAction) -> (&lsp_types::Uri, &lsp_types::TextEdit) {
    let edit = action.edit.as_ref().expect("expected workspace edit");
    let changes: &HashMap<lsp_types::Uri, Vec<lsp_types::TextEdit>> = edit
        .changes
        .as_ref()
        .expect("expected changes (not document_changes)");
    let (uri, edits) = changes
        .iter()
        .next()
        .expect("expected at least one file edit");
    let edit = edits.first().expect("expected at least one text edit");
    (uri, edit)
}

#[test]
fn type_mismatch_offers_string_value_of_quickfix() {
    let source = r#"
class A {
  void m() {
    Object o = null;
    String s = o;
  }
}
"#;

    let mut db = InMemoryFileStore::new();
    let path = PathBuf::from("/test.java");
    let file = db.file_id_for_path(path);
    db.set_file_text(file, source.to_string());

    let needle = "String s = o;";
    let stmt_start = source.find(needle).expect("expected assignment in fixture");
    let expr_start = stmt_start + "String s = ".len();
    let expr_end = expr_start + "o".len();
    let selection = Span::new(expr_start, expr_end);

    let db: Arc<InMemoryFileStore> = Arc::new(db);
    let ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), ProjectId::new(0));
    let actions = ide.code_actions_lsp(CancellationToken::new(), file, Some(selection));

    let mut quickfixes = actions.iter().filter_map(|action| match action {
        lsp_types::CodeActionOrCommand::CodeAction(action)
            if action.kind == Some(lsp_types::CodeActionKind::QUICKFIX) =>
        {
            Some(action)
        }
        _ => None,
    });

    let string_fix = quickfixes
        .find(|action| action.title == "Convert to String")
        .expect("expected Convert to String quickfix");
    assert_eq!(string_fix.is_preferred, Some(true));
    let (_uri, edit) = first_edit(string_fix);
    assert_eq!(edit.new_text, "String.valueOf(o)");

    // Ensure the edit actually rewrites the assignment expression.
    let updated = apply_lsp_edits(source, std::slice::from_ref(edit));
    assert!(
        updated.contains("String s = String.valueOf(o);"),
        "expected assignment to be rewritten; got:\n{updated}"
    );

    // Also ensure the cast quickfix is still offered alongside the conversion.
    let cast_fix = actions.iter().find_map(|action| match action {
        lsp_types::CodeActionOrCommand::CodeAction(action)
            if action.kind == Some(lsp_types::CodeActionKind::QUICKFIX)
                && action.title == "Cast to String" =>
        {
            Some(action)
        }
        _ => None,
    });
    let cast_fix = cast_fix.expect("expected Cast to String quickfix");
    assert_eq!(cast_fix.is_preferred, Some(false));
    let (_uri, edit) = first_edit(cast_fix);
    assert_eq!(edit.new_text, "(String) o");
}

#[test]
fn type_mismatch_offers_quickfix_for_cursor_at_span_start() {
    let source = r#"
class A {
  void m() {
    Object o = null;
    String s = o;
  }
}
"#;

    let mut db = InMemoryFileStore::new();
    let path = PathBuf::from("/test.java");
    let file = db.file_id_for_path(path);
    db.set_file_text(file, source.to_string());

    let needle = "String s = o;";
    let stmt_start = source.find(needle).expect("expected assignment in fixture");
    let expr_start = stmt_start + "String s = ".len();
    let selection = Span::new(expr_start, expr_start);

    let db: Arc<InMemoryFileStore> = Arc::new(db);
    let ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), ProjectId::new(0));
    let actions = ide.code_actions_lsp(CancellationToken::new(), file, Some(selection));

    let cast_fix = actions.iter().find_map(|action| match action {
        lsp_types::CodeActionOrCommand::CodeAction(action)
            if action.kind == Some(lsp_types::CodeActionKind::QUICKFIX)
                && action.title == "Cast to String" =>
        {
            Some(action)
        }
        _ => None,
    });

    let cast_fix = cast_fix.expect("expected Cast to String quickfix at cursor boundary");
    assert!(
        cast_fix.edit.is_some(),
        "expected cast quickfix to include an edit"
    );
}

#[test]
fn type_mismatch_offers_quickfix_for_cursor_at_span_end() {
    let source = r#"
class A {
  void m() {
    Object o = null;
    String s = o;
  }
}
"#;

    let mut db = InMemoryFileStore::new();
    let path = PathBuf::from("/test.java");
    let file = db.file_id_for_path(path);
    db.set_file_text(file, source.to_string());

    let needle = "String s = o;";
    let stmt_start = source.find(needle).expect("expected assignment in fixture");
    let expr_start = stmt_start + "String s = ".len();
    let expr_end = expr_start + "o".len();

    // Cursor selection (zero-length span) at the end of the mismatched expression.
    let selection = Span::new(expr_end, expr_end);

    let db: Arc<InMemoryFileStore> = Arc::new(db);
    let ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), ProjectId::new(0));
    let actions = ide.code_actions_lsp(CancellationToken::new(), file, Some(selection));

    let cast_fix = actions.iter().find_map(|action| match action {
        lsp_types::CodeActionOrCommand::CodeAction(action)
            if action.kind == Some(lsp_types::CodeActionKind::QUICKFIX)
                && action.title == "Cast to String" =>
        {
            Some(action)
        }
        _ => None,
    });

    let cast_fix = cast_fix.expect("expected Cast to String quickfix at cursor boundary");
    assert!(
        cast_fix.edit.is_some(),
        "expected cast quickfix to include an edit"
    );
}

#[test]
fn type_mismatch_offers_string_value_of_quickfix_for_primitive() {
    let source = r#"
class A {
  void m() {
    int i = 42;
    String s = i;
  }
}
"#;

    let mut db = InMemoryFileStore::new();
    let path = PathBuf::from("/test.java");
    let file = db.file_id_for_path(path);
    db.set_file_text(file, source.to_string());

    let needle = "String s = i;";
    let stmt_start = source.find(needle).expect("expected assignment in fixture");
    let expr_start = stmt_start + "String s = ".len();
    let expr_end = expr_start + "i".len();
    let selection = Span::new(expr_start, expr_end);

    let db: Arc<InMemoryFileStore> = Arc::new(db);
    let ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), ProjectId::new(0));
    let actions = ide.code_actions_lsp(CancellationToken::new(), file, Some(selection));

    let string_fix = actions.iter().find_map(|action| match action {
        lsp_types::CodeActionOrCommand::CodeAction(action)
            if action.kind == Some(lsp_types::CodeActionKind::QUICKFIX)
                && action.title == "Convert to String" =>
        {
            Some(action)
        }
        _ => None,
    });
    let string_fix = string_fix.expect("expected Convert to String quickfix");
    assert_eq!(string_fix.is_preferred, Some(true));
    let (_uri, edit) = first_edit(string_fix);
    assert_eq!(edit.new_text, "String.valueOf(i)");

    let updated = apply_lsp_edits(source, std::slice::from_ref(edit));
    assert!(
        updated.contains("String s = String.valueOf(i);"),
        "expected assignment to be rewritten; got:\n{updated}"
    );
}

#[test]
fn type_mismatch_cast_wraps_binary_expression_in_parentheses() {
    let source = r#"
class A {
  void m() {
    int a = 1;
    int b = 2;
    byte c = a + b;
  }
}
"#;

    let mut db = InMemoryFileStore::new();
    let path = PathBuf::from("/test.java");
    let file = db.file_id_for_path(path);
    db.set_file_text(file, source.to_string());

    let needle = "byte c = a + b;";
    let stmt_start = source
        .find(needle)
        .expect("expected assignment with binary expression in fixture");
    let expr_start = stmt_start + "byte c = ".len();
    let expr_end = expr_start + "a + b".len();
    let selection = Span::new(expr_start, expr_end);

    let db: Arc<InMemoryFileStore> = Arc::new(db);
    let ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), ProjectId::new(0));
    let actions = ide.code_actions_lsp(CancellationToken::new(), file, Some(selection));

    let cast_fix = actions.iter().find_map(|action| match action {
        lsp_types::CodeActionOrCommand::CodeAction(action)
            if action.kind == Some(lsp_types::CodeActionKind::QUICKFIX)
                && action.title == "Cast to byte" =>
        {
            Some(action)
        }
        _ => None,
    });
    let cast_fix = cast_fix.expect("expected Cast to byte quickfix");
    let (_uri, edit) = first_edit(cast_fix);
    assert_eq!(edit.new_text, "(byte) (a + b)");
}

#[test]
fn type_mismatch_cast_wraps_bitwise_expression_in_parentheses() {
    let source = r#"
class A {
  void m() {
    int a = 1;
    int b = 2;
    byte c = a|b;
  }
}
"#;

    let mut db = InMemoryFileStore::new();
    let path = PathBuf::from("/test.java");
    let file = db.file_id_for_path(path);
    db.set_file_text(file, source.to_string());

    let needle = "byte c = a|b;";
    let stmt_start = source
        .find(needle)
        .expect("expected assignment with bitwise expression in fixture");
    let expr_start = stmt_start + "byte c = ".len();
    let expr_end = expr_start + "a|b".len();
    let selection = Span::new(expr_start, expr_end);

    let db: Arc<InMemoryFileStore> = Arc::new(db);
    let ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), ProjectId::new(0));
    let actions = ide.code_actions_lsp(CancellationToken::new(), file, Some(selection));

    let cast_fix = actions.iter().find_map(|action| match action {
        lsp_types::CodeActionOrCommand::CodeAction(action)
            if action.kind == Some(lsp_types::CodeActionKind::QUICKFIX)
                && action.title == "Cast to byte" =>
        {
            Some(action)
        }
        _ => None,
    });
    let cast_fix = cast_fix.expect("expected Cast to byte quickfix");
    let (_uri, edit) = first_edit(cast_fix);
    assert_eq!(edit.new_text, "(byte) (a|b)");
}
