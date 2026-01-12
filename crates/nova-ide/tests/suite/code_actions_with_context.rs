use std::path::PathBuf;
use std::sync::Arc;

use crate::framework_harness::offset_to_position;
use lsp_types::{DiagnosticSeverity, NumberOrString, Range};
use nova_config::NovaConfig;
use nova_db::InMemoryFileStore;
use nova_ext::{ProjectId, Span};
use nova_ide::extensions::IdeExtensions;
use nova_scheduler::CancellationToken;

fn first_edit_new_text(action: &lsp_types::CodeAction) -> &str {
    let edit = action.edit.as_ref().expect("expected workspace edit");
    let changes = edit
        .changes
        .as_ref()
        .expect("expected WorkspaceEdit.changes");
    let (_, edits) = changes
        .iter()
        .next()
        .expect("expected at least one file edit");
    assert_eq!(edits.len(), 1, "expected exactly one text edit");
    edits[0].new_text.as_str()
}

#[test]
fn code_actions_with_context_includes_unused_import_quickfix_for_cursor_selection() {
    let mut db = InMemoryFileStore::new();
    let file = db.file_id_for_path(PathBuf::from("/test.java"));
    let source = "import java.util.List;\nclass A {}\n";
    db.set_file_text(file, source.to_string());

    let diag_start = 0;
    let diag_end = source
        .find('\n')
        .expect("import line should end with newline");
    let range = Range::new(
        offset_to_position(source, diag_start),
        offset_to_position(source, diag_end),
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

    // The LSP can request code actions for a cursor selection (start == end). Ensure we still
    // surface the unused-import quick fix when the cursor is at/inside the diagnostic range.
    let actions = ide.code_actions_lsp_with_context(
        CancellationToken::new(),
        file,
        Some(Span::new(diag_start, diag_start)),
        &[diag],
    );

    assert!(
        actions.iter().any(|action| match action {
            lsp_types::CodeActionOrCommand::CodeAction(action)
                if action.title == "Remove unused import"
                    && action.kind == Some(lsp_types::CodeActionKind::QUICKFIX) =>
            {
                true
            }
            _ => false,
        }),
        "expected to find `Remove unused import` quick fix; got {actions:?}"
    );
}

#[test]
fn code_actions_with_context_includes_unresolved_import_quickfix_for_cursor_selection() {
    let mut db = InMemoryFileStore::new();
    let file = db.file_id_for_path(PathBuf::from("/test.java"));
    let source = "import does.not.Exist;\nclass A {}\n";
    db.set_file_text(file, source.to_string());

    let diag_start = 0;
    let diag_end = source
        .find('\n')
        .expect("import line should end with newline");
    let range = Range::new(
        offset_to_position(source, diag_start),
        offset_to_position(source, diag_end),
    );

    let diag = lsp_types::Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String("unresolved-import".to_string())),
        message: "unresolved import".to_string(),
        ..lsp_types::Diagnostic::default()
    };

    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(db);
    let ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), ProjectId::new(0));

    let actions = ide.code_actions_lsp_with_context(
        CancellationToken::new(),
        file,
        Some(Span::new(diag_start, diag_start)),
        &[diag],
    );

    assert!(
        actions.iter().any(|action| match action {
            lsp_types::CodeActionOrCommand::CodeAction(action)
                if action.title == "Remove unresolved import"
                    && action.kind == Some(lsp_types::CodeActionKind::QUICKFIX) =>
            {
                true
            }
            _ => false,
        }),
        "expected to find `Remove unresolved import` quick fix; got {actions:?}"
    );
}

#[test]
fn code_actions_with_context_includes_type_mismatch_quickfix() {
    let mut db = InMemoryFileStore::new();
    let file = db.file_id_for_path(PathBuf::from("/test.java"));
    let source = r#"class A {
  void m() {
    Object obj = new Object();
    String s = obj;
  }
}
"#;
    db.set_file_text(file, source.to_string());

    let expr_start = source.rfind("obj;").expect("expression span");
    let expr_end = expr_start + "obj".len();

    let range = Range::new(
        offset_to_position(source, expr_start),
        offset_to_position(source, expr_end),
    );

    let diag = lsp_types::Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String("type-mismatch".to_string())),
        message: "type mismatch: expected String, found Object".to_string(),
        ..lsp_types::Diagnostic::default()
    };

    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(db);
    let ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), ProjectId::new(0));

    let actions = ide.code_actions_lsp_with_context(
        CancellationToken::new(),
        file,
        Some(Span::new(expr_start, expr_end)),
        &[diag],
    );

    assert!(
        actions.iter().any(|action| match action {
            lsp_types::CodeActionOrCommand::CodeAction(action)
                if action.title == "Cast to String"
                    && action.kind == Some(lsp_types::CodeActionKind::QUICKFIX) =>
            {
                true
            }
            _ => false,
        }),
        "expected to find `Cast to String` quick fix; got {actions:?}"
    );
}

#[test]
fn code_actions_with_context_includes_type_mismatch_quickfix_for_cursor_at_span_start() {
    let mut db = InMemoryFileStore::new();
    let file = db.file_id_for_path(PathBuf::from("/test.java"));
    let source = r#"class A {
  void m() {
    Object obj = new Object();
    String s = obj;
  }
}
"#;
    db.set_file_text(file, source.to_string());

    let expr_start = source.rfind("obj;").expect("expression span");
    let expr_end = expr_start + "obj".len();

    let range = Range::new(
        offset_to_position(source, expr_start),
        offset_to_position(source, expr_end),
    );

    let diag = lsp_types::Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String("type-mismatch".to_string())),
        message: "type mismatch: expected String, found Object".to_string(),
        ..lsp_types::Diagnostic::default()
    };

    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(db);
    let ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), ProjectId::new(0));

    let actions = ide.code_actions_lsp_with_context(
        CancellationToken::new(),
        file,
        Some(Span::new(expr_start, expr_start)),
        &[diag],
    );

    assert!(
        actions.iter().any(|action| match action {
            lsp_types::CodeActionOrCommand::CodeAction(action)
                if action.title == "Cast to String"
                    && action.kind == Some(lsp_types::CodeActionKind::QUICKFIX) =>
            {
                true
            }
            _ => false,
        }),
        "expected to find `Cast to String` quick fix at cursor boundary; got {actions:?}"
    );
}

#[test]
fn code_actions_with_context_cast_wraps_binary_expression_in_parentheses() {
    let mut db = InMemoryFileStore::new();
    let file = db.file_id_for_path(PathBuf::from("/test.java"));
    let source = r#"class A {
  void m() {
    int a = 1;
    int b = 2;
    byte c = a + b;
  }
}
"#;
    db.set_file_text(file, source.to_string());

    let stmt_start = source
        .find("byte c = a + b;")
        .expect("expected assignment in fixture");
    let expr_start = stmt_start + "byte c = ".len();
    let expr_end = expr_start + "a + b".len();

    let range = Range::new(
        offset_to_position(source, expr_start),
        offset_to_position(source, expr_end),
    );

    let diag = lsp_types::Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String("type-mismatch".to_string())),
        message: "type mismatch: expected byte, found int".to_string(),
        ..lsp_types::Diagnostic::default()
    };

    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(db);
    let ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), ProjectId::new(0));

    let actions = ide.code_actions_lsp_with_context(
        CancellationToken::new(),
        file,
        Some(Span::new(expr_start, expr_end)),
        &[diag],
    );

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
    assert_eq!(first_edit_new_text(cast_fix), "(byte) (a + b)");
}

#[test]
fn code_actions_with_context_cast_wraps_bitwise_expression_in_parentheses() {
    let mut db = InMemoryFileStore::new();
    let file = db.file_id_for_path(PathBuf::from("/test.java"));
    let source = r#"class A {
  void m() {
    int a = 1;
    int b = 2;
    byte c = a|b;
  }
}
"#;
    db.set_file_text(file, source.to_string());

    let stmt_start = source
        .find("byte c = a|b;")
        .expect("expected assignment in fixture");
    let expr_start = stmt_start + "byte c = ".len();
    let expr_end = expr_start + "a|b".len();

    let range = Range::new(
        offset_to_position(source, expr_start),
        offset_to_position(source, expr_end),
    );

    let diag = lsp_types::Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String("type-mismatch".to_string())),
        message: "type mismatch: expected byte, found int".to_string(),
        ..lsp_types::Diagnostic::default()
    };

    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(db);
    let ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), ProjectId::new(0));

    let actions = ide.code_actions_lsp_with_context(
        CancellationToken::new(),
        file,
        Some(Span::new(expr_start, expr_end)),
        &[diag],
    );

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
    assert_eq!(first_edit_new_text(cast_fix), "(byte) (a|b)");
}

#[test]
fn code_actions_with_context_includes_return_mismatch_remove_returned_value_quickfix_for_cursor_at_span_start(
) {
    let mut db = InMemoryFileStore::new();
    let file = db.file_id_for_path(PathBuf::from("/test.java"));
    let source = r#"class A {
  void m() {
    return 1;
  }
}
"#;
    db.set_file_text(file, source.to_string());

    let expr_start = source.rfind("1;").expect("expression span");
    let expr_end = expr_start + "1".len();

    let range = Range::new(
        offset_to_position(source, expr_start),
        offset_to_position(source, expr_end),
    );

    let diag = lsp_types::Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String("return-mismatch".to_string())),
        message: "cannot return a value from a `void` method".to_string(),
        ..lsp_types::Diagnostic::default()
    };

    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(db);
    let ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), ProjectId::new(0));

    let actions = ide.code_actions_lsp_with_context(
        CancellationToken::new(),
        file,
        Some(Span::new(expr_start, expr_start)),
        &[diag],
    );

    let fix = actions.iter().find_map(|action| match action {
        lsp_types::CodeActionOrCommand::CodeAction(action)
            if action.kind == Some(lsp_types::CodeActionKind::QUICKFIX)
                && action.title == "Remove returned value" =>
        {
            Some(action)
        }
        _ => None,
    });
    let fix = fix.expect("expected Remove returned value quickfix at cursor boundary");
    assert_eq!(first_edit_new_text(fix), "");
}

#[test]
fn code_actions_with_context_includes_return_mismatch_quickfix_for_cursor_at_span_start() {
    let mut db = InMemoryFileStore::new();
    let file = db.file_id_for_path(PathBuf::from("/test.java"));
    let source = r#"class A {
  String m() {
    Object o = "";
    return o;
  }
}
"#;
    db.set_file_text(file, source.to_string());

    let stmt_start = source.find("return o;").expect("expected return statement");
    let expr_start = stmt_start + "return ".len();
    let expr_end = expr_start + "o".len();

    let range = Range::new(
        offset_to_position(source, expr_start),
        offset_to_position(source, expr_end),
    );

    let diag = lsp_types::Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String("return-mismatch".to_string())),
        message: "return type mismatch: expected String, found Object".to_string(),
        ..lsp_types::Diagnostic::default()
    };

    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(db);
    let ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), ProjectId::new(0));

    let actions = ide.code_actions_lsp_with_context(
        CancellationToken::new(),
        file,
        Some(Span::new(expr_start, expr_start)),
        &[diag],
    );

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
    assert_eq!(first_edit_new_text(cast_fix), "(String) (o)");
}
