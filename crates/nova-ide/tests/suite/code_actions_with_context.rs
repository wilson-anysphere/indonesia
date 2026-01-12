use std::path::PathBuf;
use std::sync::Arc;

use crate::text_fixture::offset_to_position;
use lsp_types::{DiagnosticSeverity, NumberOrString, Range};
use nova_config::NovaConfig;
use nova_db::InMemoryFileStore;
use nova_ext::{ProjectId, Span};
use nova_ide::extensions::IdeExtensions;
use nova_scheduler::CancellationToken;

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
