use std::path::PathBuf;
use std::sync::Arc;

use lsp_types::{CodeActionOrCommand, DiagnosticSeverity, NumberOrString, Position, Range};
use nova_config::NovaConfig;
use nova_db::{InMemoryFileStore, SalsaDbView};
use nova_ext::{ProjectId, Span};
use nova_ide::code_action::diagnostic_quick_fixes;
use nova_ide::extensions::IdeExtensions;
use nova_scheduler::CancellationToken;
use nova_types::Severity;

use crate::framework_harness::{offset_to_position, position_to_offset};

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
    let (_uri, edits) = changes
        .iter()
        .next()
        .expect("expected at least one file edit");
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

fn ide_for_source(
    path: &str,
    source: &str,
) -> (
    IdeExtensions<dyn nova_db::Database + Send + Sync>,
    nova_db::FileId,
) {
    let mut db = InMemoryFileStore::new();
    let path = PathBuf::from(path);
    let file = db.file_id_for_path(&path);
    db.set_file_text(file, source.to_string());

    // `IdeExtensions` requires a `Send + Sync` database; wrap our in-memory store in a
    // snapshot-like view.
    let view = SalsaDbView::from_source_db(&db);
    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(view);
    let ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), ProjectId::new(0));

    (ide, file)
}

#[test]
fn unresolved_name_offers_create_variable_and_field_quick_fixes() {
    let source = "class A {\n  void m() {\n    int x = y;\n  }\n}\n";

    let (ide, file) = ide_for_source("/test.java", source);

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
    let source = r#"class A { void m() { return 1; } }"#;
    let (ide, file) = ide_for_source("/return_void.java", source);

    let expr_start = source.find("return 1").expect("expected return statement") + "return ".len();
    let expr_span = Span::new(expr_start, expr_start + 1);

    let actions = ide.code_actions_lsp(CancellationToken::new(), file, Some(expr_span));
    let action =
        find_code_action(&actions, "Remove returned value").expect("missing quick fix action");
    assert_eq!(
        action.kind.as_ref(),
        Some(&lsp_types::CodeActionKind::QUICKFIX)
    );

    let updated = apply_lsp_edits(source, std::slice::from_ref(first_text_edit(action)));
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
fn code_actions_with_context_void_method_return_value_offers_remove_returned_value_quickfix_for_cursor_at_span_start(
) {
    let source = r#"class A { void m() { return 1; } }"#.to_string();

    let mut db = InMemoryFileStore::new();
    let path = PathBuf::from("/test.java");
    let file = db.file_id_for_path(&path);
    db.set_file_text(file, source.clone());

    let view = SalsaDbView::from_source_db(&db);
    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(view);
    let ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), ProjectId::new(0));

    let expr_start = source.find("return 1").expect("expected return statement") + "return ".len();
    let expr_end = expr_start + 1;

    let diag = lsp_types::Diagnostic {
        range: Range::new(
            offset_to_position(&source, expr_start),
            offset_to_position(&source, expr_end),
        ),
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String("return-mismatch".to_string())),
        message: "cannot return a value from a `void` method".to_string(),
        ..lsp_types::Diagnostic::default()
    };

    let selection = Span::new(expr_start, expr_start);
    let actions =
        ide.code_actions_lsp_with_context(CancellationToken::new(), file, Some(selection), &[diag]);
    let action = find_code_action(&actions, "Remove returned value").unwrap_or_else(|| {
        panic!(
            "missing `Remove returned value` quick fix; got titles {:?}",
            action_titles(&actions)
        )
    });

    assert_eq!(
        action.kind.as_ref(),
        Some(&lsp_types::CodeActionKind::QUICKFIX)
    );

    let updated = apply_workspace_edit(&source, action.edit.as_ref().expect("expected edit"));
    assert!(
        updated.contains("return ;"),
        "expected returned value to be removed; got {updated:?}"
    );
}

#[test]
fn return_type_mismatch_offers_cast_quickfix() {
    let source = r#"class A { String m() { Object o = ""; return o; } }"#;
    let (ide, file) = ide_for_source("/return_cast.java", source);

    let expr_start = source.find("return o").expect("expected return statement") + "return ".len();
    let expr_span = Span::new(expr_start, expr_start + 1);

    let actions = ide.code_actions_lsp(CancellationToken::new(), file, Some(expr_span));
    let action = find_code_action(&actions, "Cast to String").expect("missing cast quick fix");
    let updated = apply_lsp_edits(source, std::slice::from_ref(first_text_edit(action)));
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
    let (ide, file) = ide_for_source("/create_class.java", source);

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
    let (ide, file) = ide_for_source("/field_inline.java", source);

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

#[test]
fn unresolved_name_type_like_offers_import_and_qualify_quick_fixes() {
    let source = r#"
class A {
  void m() {
    List.of("x");
  }
}
"#;

    let mut db = InMemoryFileStore::new();
    let path = PathBuf::from("/test.java");
    let file = db.file_id_for_path(&path);
    db.set_file_text(file, source.to_string());

    // `IdeExtensions` requires a `Send + Sync` database; wrap our in-memory store in a
    // snapshot-like view.
    let view = SalsaDbView::from_source_db(&db);
    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(view);
    let ide = IdeExtensions::new(
        Arc::clone(&db),
        Arc::new(NovaConfig::default()),
        ProjectId::new(0),
    );

    // Ensure `List` triggers an `unresolved-name` diagnostic (expression position).
    let cancel = CancellationToken::new();
    let diagnostics = nova_ide::core_file_diagnostics(db.as_ref(), file, &cancel);
    assert!(
        diagnostics.iter().any(|d| {
            d.severity == Severity::Error
                && d.code.as_ref() == "unresolved-name"
                && d.message.contains("List")
        }),
        "expected unresolved-name diagnostic for List; got {diagnostics:#?}"
    );

    let list_start = source.find("List").expect("expected List in fixture");
    let list_end = list_start + "List".len();
    let list_span = Span::new(list_start, list_end);

    let actions = ide.code_actions_lsp(CancellationToken::new(), file, Some(list_span));
    let titles: Vec<&str> = actions
        .iter()
        .filter_map(|action| match action {
            CodeActionOrCommand::CodeAction(action) => Some(action.title.as_str()),
            CodeActionOrCommand::Command(cmd) => Some(cmd.title.as_str()),
        })
        .collect();

    assert!(
        titles.iter().any(|t| *t == "Import java.util.List"),
        "expected Import quick fix; got {titles:?}"
    );
    assert!(
        titles
            .iter()
            .any(|t| *t == "Use fully qualified name 'java.util.List'"),
        "expected fully qualified name quick fix; got {titles:?}"
    );
}

#[test]
fn unresolved_name_type_like_offers_create_class_quick_fix() {
    let source = r#"
class A {
  void m() {
    MissingType.of();
  }
}
"#;

    let mut db = InMemoryFileStore::new();
    let path = PathBuf::from("/test.java");
    let file = db.file_id_for_path(&path);
    db.set_file_text(file, source.to_string());

    let view = SalsaDbView::from_source_db(&db);
    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(view);
    let ide = IdeExtensions::new(
        Arc::clone(&db),
        Arc::new(NovaConfig::default()),
        ProjectId::new(0),
    );

    // Ensure `MissingType` triggers an `unresolved-name` diagnostic (expression position).
    let cancel = CancellationToken::new();
    let diagnostics = nova_ide::core_file_diagnostics(db.as_ref(), file, &cancel);
    assert!(
        diagnostics.iter().any(|d| {
            d.severity == Severity::Error
                && d.code.as_ref() == "unresolved-name"
                && d.message.contains("MissingType")
        }),
        "expected unresolved-name diagnostic for MissingType; got {diagnostics:#?}"
    );

    let start = source
        .find("MissingType")
        .expect("expected MissingType in fixture");
    let end = start + "MissingType".len();
    let span = Span::new(start, end);

    let actions = ide.code_actions_lsp(CancellationToken::new(), file, Some(span));
    assert!(
        actions.iter().any(|action| match action {
            CodeActionOrCommand::CodeAction(action) => action.title == "Create class 'MissingType'",
            CodeActionOrCommand::Command(_) => false,
        }),
        "expected Create class quick fix; got titles {:?}",
        action_titles(&actions)
    );
}

#[test]
fn type_mismatch_offers_cast_quick_fix() {
    let source = r#"class A {
  void m() {
    Object o = "";
    String s = o;
  }
}
"#;

    let (ide, file) = ide_for_source("/type_mismatch.java", source);

    let stmt = "String s = o;";
    let stmt_start = source.find(stmt).expect("missing assignment statement");
    let o_start = stmt_start
        + source[stmt_start..]
            .find("o;")
            .expect("missing `o;` in assignment");
    let span = Span::new(o_start, o_start + 1);

    let actions = ide.code_actions_lsp(CancellationToken::new(), file, Some(span));

    let cast = actions
        .iter()
        .filter_map(|action| match action {
            CodeActionOrCommand::CodeAction(action) => Some(action),
            CodeActionOrCommand::Command(_) => None,
        })
        .find(|action| {
            action.kind.as_ref() == Some(&lsp_types::CodeActionKind::QUICKFIX)
                && action.title.contains("Cast")
        })
        .expect("expected cast quick fix");

    let edit = cast.edit.as_ref().expect("expected workspace edit");
    let changes = edit.changes.as_ref().expect("expected edit.changes");
    let edits = changes.values().next().expect("expected edit entry");
    assert_eq!(edits.len(), 1, "expected a single text edit; got {edits:?}");
    assert!(
        edits[0].new_text.contains("(String)"),
        "expected cast to String; got {:?}",
        edits[0].new_text
    );
    assert!(
        edits[0].new_text.contains('o'),
        "expected original expr to be preserved; got {:?}",
        edits[0].new_text
    );
}

#[test]
fn unresolved_method_offers_jdk_static_member_quick_fixes() {
    let source = "class A { void m() { int x = max(1, 2); } }";
    let (ide, file) = ide_for_source("/static_member_method.java", source);

    let max_start = source.find("max").expect("expected max in fixture");
    let max_span = Span::new(max_start, max_start + "max".len());

    let actions = ide.code_actions_lsp(CancellationToken::new(), file, Some(max_span));
    let titles = action_titles(&actions);
    assert!(
        titles.iter().any(|t| *t == "Qualify with Math"),
        "expected qualify quick fix; got {titles:?}"
    );
    assert!(
        titles
            .iter()
            .any(|t| *t == "Add static import java.lang.Math.max"),
        "expected static import quick fix; got {titles:?}"
    );

    let qualify =
        find_code_action(&actions, "Qualify with Math").expect("missing qualify quick fix");
    assert_eq!(first_text_edit(qualify).new_text, "Math.max");

    let import = find_code_action(&actions, "Add static import java.lang.Math.max")
        .expect("missing static import quick fix");
    let updated = apply_workspace_edit(source, import.edit.as_ref().expect("expected edit"));
    assert!(
        updated.contains("import static java.lang.Math.max;"),
        "expected static import insertion; got:\n{updated}"
    );
}

#[test]
fn unresolved_import_offers_remove_quick_fix() {
    let source = "import foo.Bar;\nclass A {}\n";
    let (ide, file) = ide_for_source("/imports.java", source);

    let needle = "foo.Bar";
    let start = source.find(needle).expect("missing import path");
    let span = Span::new(start, start + 1);

    let actions = ide.code_actions_lsp(CancellationToken::new(), file, Some(span));
    let action = find_code_action(&actions, "Remove unresolved import")
        .expect("expected remove import quick fix");

    assert_eq!(
        action.kind.as_ref(),
        Some(&lsp_types::CodeActionKind::QUICKFIX)
    );
    let edit = action.edit.as_ref().expect("expected workspace edit");

    let updated = apply_workspace_edit(source, edit);
    assert_eq!(updated, "class A {}\n");
}

#[test]
fn unresolved_type_offers_import_quick_fix() {
    let source = "class A { List<String> xs; }\n";
    let (ide, file) = ide_for_source("/types.java", source);

    let start = source.find("List").expect("missing `List`");
    let span = Span::new(start, start + "List".len());

    let actions = ide.code_actions_lsp(CancellationToken::new(), file, Some(span));
    let action = find_code_action(&actions, "Import java.util.List")
        .expect("expected import java.util.List quick fix");

    assert_eq!(
        action.kind.as_ref(),
        Some(&lsp_types::CodeActionKind::QUICKFIX)
    );
    let edit = action.edit.as_ref().expect("expected workspace edit");

    let updated = apply_workspace_edit(source, edit);
    assert!(
        updated.contains("import java.util.List;"),
        "expected updated source to contain import; got:\n{updated}"
    );
}

#[test]
fn quick_fixes_are_filtered_by_requested_span() {
    let source = "class A { List<String> xs; }\n";
    let (ide, file) = ide_for_source("/filter.java", source);

    let class_start = source.find("class").expect("missing `class`");
    let span = Span::new(class_start, class_start + "class".len());

    let actions = ide.code_actions_lsp(CancellationToken::new(), file, Some(span));
    assert!(
        find_code_action(&actions, "Import java.util.List").is_none(),
        "expected import quick fix to be filtered out; got {:?}",
        action_titles(&actions)
    );
}

#[test]
fn unresolved_name_offers_jdk_static_member_quick_fixes() {
    let source = "class A { double x = PI; }";
    let (ide, file) = ide_for_source("/static_member_field.java", source);

    let pi_start = source.find("PI").expect("expected PI in fixture");
    let pi_span = Span::new(pi_start, pi_start + "PI".len());

    let actions = ide.code_actions_lsp(CancellationToken::new(), file, Some(pi_span));
    let titles = action_titles(&actions);
    assert!(
        titles.iter().any(|t| *t == "Qualify with Math"),
        "expected qualify quick fix; got {titles:?}"
    );

    let qualify =
        find_code_action(&actions, "Qualify with Math").expect("missing qualify quick fix");
    assert_eq!(first_text_edit(qualify).new_text, "Math.PI");
}

#[test]
fn diagnostic_quick_fixes_offer_create_variable_and_field_for_unresolved_name() {
    let source = "class A {\n  void m() {\n    int x = y;\n  }\n}\n";
    let uri: lsp_types::Uri = "file:///test.java".parse().expect("valid uri");

    let y_offset = source.find("y;").expect("expected `y` in fixture");
    let range = Range::new(
        offset_to_position(source, y_offset),
        offset_to_position(source, y_offset + 1),
    );

    let diag = lsp_types::Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String("unresolved-name".to_string())),
        message: "unresolved reference `y`".to_string(),
        ..lsp_types::Diagnostic::default()
    };

    let actions = diagnostic_quick_fixes(source, Some(uri), range, &[diag]);
    let local = actions
        .iter()
        .find(|action| action.title == "Create local variable 'y'")
        .expect("expected Create local variable quick fix");
    let field = actions
        .iter()
        .find(|action| action.title == "Create field 'y'")
        .expect("expected Create field quick fix");

    let updated = apply_workspace_edit(source, local.edit.as_ref().expect("expected edit"));
    assert!(
        updated.contains("    Object y = null;\n    int x = y;"),
        "expected local-variable stub before statement; got:\n{updated}"
    );
    assert_eq!(first_text_edit(local).range.start, Position::new(2, 0));

    let updated = apply_workspace_edit(source, field.edit.as_ref().expect("expected edit"));
    assert!(
        updated.contains("  private Object y;\n}"),
        "expected field stub before final brace; got:\n{updated}"
    );
    assert_eq!(first_text_edit(field).range.start, Position::new(4, 0));
}

#[test]
fn diagnostic_quick_fixes_create_field_in_single_line_file_inserts_before_final_brace() {
    let source = "class A { void m() { int x = y; } }";
    let uri: lsp_types::Uri = "file:///test.java".parse().expect("valid uri");

    let y_offset = source.find("y;").expect("expected `y` in fixture");
    let range = Range::new(
        offset_to_position(source, y_offset),
        offset_to_position(source, y_offset + 1),
    );

    let diag = lsp_types::Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String("unresolved-name".to_string())),
        message: "unresolved reference `y`".to_string(),
        ..lsp_types::Diagnostic::default()
    };

    let actions = diagnostic_quick_fixes(source, Some(uri), range, &[diag]);
    let field = actions
        .iter()
        .find(|action| action.title == "Create field 'y'")
        .expect("expected Create field quick fix");

    let updated = apply_workspace_edit(source, field.edit.as_ref().expect("expected edit"));
    assert!(
        updated.contains("private Object y;"),
        "expected inserted field; got:\n{updated}"
    );
    assert!(
        updated.ends_with("\n}"),
        "expected inserted field to end with closing brace on its own line; got:\n{updated}"
    );
}

#[test]
fn diagnostic_quick_fixes_unresolved_method_offers_jdk_static_member_quick_fixes() {
    let source = "class A { void m() { int x = max(1, 2); } }";
    let uri: lsp_types::Uri = "file:///test.java".parse().expect("valid uri");

    let max_start = source.find("max").expect("expected max in fixture");
    let max_end = max_start + "max".len();
    let range = Range::new(
        offset_to_position(source, max_start),
        offset_to_position(source, max_end),
    );

    let diag = lsp_types::Diagnostic {
        range: range.clone(),
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String("unresolved-method".to_string())),
        message: "unresolved reference `max`".to_string(),
        ..lsp_types::Diagnostic::default()
    };

    let actions = diagnostic_quick_fixes(source, Some(uri), range, &[diag]);
    let titles: Vec<&str> = actions.iter().map(|a| a.title.as_str()).collect();

    assert!(
        titles.iter().any(|t| *t == "Qualify with Math"),
        "expected qualify quick fix; got {titles:?}"
    );
    assert!(
        titles
            .iter()
            .any(|t| *t == "Add static import java.lang.Math.max"),
        "expected static import quick fix; got {titles:?}"
    );
}

#[test]
fn diagnostic_quick_fixes_static_import_inserts_import_statement() {
    let source = "class A { void m() { int x = max(1, 2); } }";
    let uri: lsp_types::Uri = "file:///test.java".parse().expect("valid uri");

    let max_start = source.find("max").expect("expected max in fixture");
    let max_end = max_start + "max".len();
    let range = Range::new(
        offset_to_position(source, max_start),
        offset_to_position(source, max_end),
    );

    let diag = lsp_types::Diagnostic {
        range: range.clone(),
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String("unresolved-method".to_string())),
        message: "unresolved reference `max`".to_string(),
        ..lsp_types::Diagnostic::default()
    };

    let actions = diagnostic_quick_fixes(source, Some(uri), range, &[diag]);
    let import = actions
        .iter()
        .find(|action| action.title == "Add static import java.lang.Math.max")
        .expect("missing static import quick fix");

    let updated = apply_workspace_edit(source, import.edit.as_ref().expect("expected edit"));
    assert!(
        updated.contains("import static java.lang.Math.max;"),
        "expected static import insertion; got:\n{updated}"
    );
}
