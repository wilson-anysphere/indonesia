use std::path::PathBuf;
use std::sync::Arc;

use crate::text_fixture::offset_to_position;
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
fn code_actions_with_context_includes_type_mismatch_quickfix() {
    let mut db = InMemoryFileStore::new();
    let file = db.file_id_for_path(PathBuf::from("/test.java"));
    let source = r#"class A {
  void m() {
    Object obj = new Object();
    String s = obj;
  }
}

#[test]
fn code_actions_with_context_includes_unused_import_quickfix_for_cursor_selection() {
    let mut db = InMemoryFileStore::new();
    let file = db.file_id_for_path(PathBuf::from("/test.java"));
    let source = "import java.util.List;\nclass A {}\n";
    db.set_file_text(file, source.to_string());

    let diag_start = 0;
    let diag_end = source.find('\n').expect("import line should end with newline");
    let range = Range::new(
        offset_to_position_utf16(source, diag_start),
        offset_to_position_utf16(source, diag_end),
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

    let mut found = false;
    for action in &actions {
        let lsp_types::CodeActionOrCommand::CodeAction(action) = action else {
            continue;
        };
        if action.title == "Remove unused import" {
            found = true;
            assert_eq!(action.kind, Some(lsp_types::CodeActionKind::QUICKFIX));
            assert!(
                action.edit.is_some(),
                "expected unused import quickfix to include an edit"
            );
        }
    }

    assert!(
        found,
        "expected to find `Remove unused import` quick fix; got {actions:?}"
    );
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

    let mut found = false;
    for action in &actions {
        let lsp_types::CodeActionOrCommand::CodeAction(action) = action else {
            continue;
        };
        if action.title == "Cast to String" {
            found = true;
            assert_eq!(action.kind, Some(lsp_types::CodeActionKind::QUICKFIX));
            assert!(
                action.edit.is_some(),
                "expected cast quickfix to include an edit"
            );
        }
    }

    assert!(
        found,
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
    String s = a + b;
  }
}
"#;
    db.set_file_text(file, source.to_string());

    let stmt_start = source
        .find("String s = a + b;")
        .expect("expected assignment in fixture");
    let expr_start = stmt_start + "String s = ".len();
    let expr_end = expr_start + "a + b".len();

    let range = Range::new(
        offset_to_position(source, expr_start),
        offset_to_position(source, expr_end),
    );

    let diag = lsp_types::Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String("type-mismatch".to_string())),
        message: "type mismatch: expected String, found int".to_string(),
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
                && action.title == "Cast to String" =>
        {
            Some(action)
        }
        _ => None,
    });
    let cast_fix = cast_fix.expect("expected Cast to String quickfix");
    assert_eq!(first_edit_new_text(cast_fix), "(String) (a + b)");
}
