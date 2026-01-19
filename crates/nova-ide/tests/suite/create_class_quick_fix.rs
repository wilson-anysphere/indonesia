use lsp_types::{
    CodeActionKind, Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range, Uri,
};
use nova_ide::code_action::diagnostic_quick_fixes;
use nova_test_utils::apply_lsp_edits;

use crate::framework_harness::offset_to_position;

#[test]
fn create_class_quick_fix_inserts_skeleton_at_eof() {
    let source = "class A { MissingType x; }";
    let uri: Uri = "file:///test.java".parse().expect("valid uri");

    let start = source.find("MissingType").expect("MissingType start");
    let end = start + "MissingType".len();
    let selection = Range::new(
        offset_to_position(source, start),
        offset_to_position(source, end),
    );

    let diagnostic = Diagnostic {
        range: selection,
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String("unresolved-type".to_string())),
        source: Some("nova".to_string()),
        message: "unresolved type `MissingType`".to_string(),
        ..Diagnostic::default()
    };

    let actions = diagnostic_quick_fixes(source, Some(uri.clone()), selection, &[diagnostic]);
    let action = actions
        .iter()
        .find(|action| action.title.contains("Create class 'MissingType'"))
        .expect("expected Create class quick fix");

    assert_eq!(action.kind, Some(CodeActionKind::QUICKFIX));

    let edit = action.edit.as_ref().expect("expected workspace edit");
    let changes = edit.changes.as_ref().expect("expected changes map");
    let edits = changes.get(&uri).expect("expected edit for file");
    assert_eq!(edits.len(), 1);
    assert!(edits[0].new_text.contains("class MissingType"));

    let eof = offset_to_position(source, source.len());
    assert_eq!(edits[0].range.start, eof);
    assert_eq!(edits[0].range.end, eof);
    assert!(
        edits[0].new_text.starts_with("\n\nclass MissingType"),
        "expected insertion to include leading blank line; got {:?}",
        edits[0].new_text
    );
}

#[test]
fn create_class_quick_fix_is_filtered_by_selection_span() {
    let source = "class A { MissingType x; }";
    let uri: Uri = "file:///test.java".parse().expect("valid uri");

    let missing_start = source.find("MissingType").expect("MissingType start");
    let missing_end = missing_start + "MissingType".len();
    let diagnostic_range = Range::new(
        offset_to_position(source, missing_start),
        offset_to_position(source, missing_end),
    );

    let diagnostic = Diagnostic {
        range: diagnostic_range,
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String("unresolved-type".to_string())),
        message: "unresolved type `MissingType`".to_string(),
        ..Diagnostic::default()
    };

    // Selection does not intersect `MissingType`.
    let selection = Range::new(Position::new(0, 0), Position::new(0, 1));
    let actions = diagnostic_quick_fixes(source, Some(uri), selection, &[diagnostic]);
    assert!(
        actions.is_empty(),
        "expected no actions for non-intersecting selection; got {actions:#?}"
    );
}

#[test]
fn create_class_quick_fix_is_not_suggested_for_qualified_names() {
    let source = "class A { foo.Bar x; }";
    let uri: Uri = "file:///test.java".parse().expect("valid uri");

    let start = source.find("foo.Bar").expect("type start");
    let end = start + "foo.Bar".len();
    let range = Range::new(
        offset_to_position(source, start),
        offset_to_position(source, end),
    );

    let diagnostic = Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String("unresolved-type".to_string())),
        message: "unresolved type `foo.Bar`".to_string(),
        ..Diagnostic::default()
    };

    let actions = diagnostic_quick_fixes(source, Some(uri), range, &[diagnostic]);
    assert!(
        actions.is_empty(),
        "expected no actions for qualified type names; got {actions:#?}"
    );
}

#[test]
fn unresolved_type_quick_fixes_include_import_and_fully_qualified_name_for_cursor_at_end() {
    let source = r#"class A {
  void m(List<String> xs) {}
 }
"#;
    let uri: Uri = "file:///test.java".parse().expect("valid uri");

    let list_start = source.find("List<String>").expect("List occurrence");
    let list_end = list_start + "List".len();

    let diagnostic = Diagnostic {
        range: Range::new(
            offset_to_position(source, list_start),
            offset_to_position(source, list_end),
        ),
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String("unresolved-type".to_string())),
        message: "unresolved type `List`".to_string(),
        ..Diagnostic::default()
    };

    // Cursor selection at the end of `List` (common when the cursor is placed after the token).
    let selection = Range::new(
        offset_to_position(source, list_end),
        offset_to_position(source, list_end),
    );

    let actions = diagnostic_quick_fixes(source, Some(uri.clone()), selection, &[diagnostic]);

    let import_action = actions
        .iter()
        .find(|action| action.title == "Import java.util.List")
        .expect("expected Import java.util.List quick fix");
    let fqn_action = actions
        .iter()
        .find(|action| action.title == "Use fully qualified name 'java.util.List'")
        .expect("expected FQN quick fix");

    assert_eq!(import_action.kind, Some(CodeActionKind::QUICKFIX));
    assert_eq!(fqn_action.kind, Some(CodeActionKind::QUICKFIX));

    let import_edit = import_action.edit.as_ref().expect("expected import edit");
    let import_changes = import_edit.changes.as_ref().expect("expected changes map");
    let import_edits = import_changes.get(&uri).expect("expected edits for file");
    let imported = apply_lsp_edits(source, import_edits);
    assert!(
        imported.contains("import java.util.List;"),
        "expected import insertion; got:\n{imported}"
    );

    let fqn_edit = fqn_action.edit.as_ref().expect("expected fqn edit");
    let fqn_changes = fqn_edit.changes.as_ref().expect("expected changes map");
    let fqn_edits = fqn_changes.get(&uri).expect("expected edits for file");
    let qualified = apply_lsp_edits(source, fqn_edits);
    assert!(
        qualified.contains("void m(java.util.List<String> xs) {}"),
        "expected fully qualified type reference; got:\n{qualified}"
    );
}

#[test]
fn unresolved_type_quick_fixes_include_import_and_fully_qualified_name_for_range_selection() {
    let source = "class A { List<String> xs; }";
    let uri: Uri = "file:///test.java".parse().expect("valid uri");

    let list_start = source.find("List<String>").expect("List occurrence");
    let list_end = list_start + "List".len();
    let range = Range::new(
        offset_to_position(source, list_start),
        offset_to_position(source, list_end),
    );

    let diagnostic = Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String("unresolved-type".to_string())),
        message: "unresolved type `List`".to_string(),
        ..Diagnostic::default()
    };

    let actions = diagnostic_quick_fixes(source, Some(uri.clone()), range, &[diagnostic]);

    let import_action = actions
        .iter()
        .find(|action| action.title == "Import java.util.List")
        .expect("expected Import java.util.List quick fix");
    let fqn_action = actions
        .iter()
        .find(|action| action.title == "Use fully qualified name 'java.util.List'")
        .expect("expected FQN quick fix");

    let import_edit = import_action.edit.as_ref().expect("expected import edit");
    let import_changes = import_edit.changes.as_ref().expect("expected changes map");
    let import_edits = import_changes.get(&uri).expect("expected edits for file");
    let imported = apply_lsp_edits(source, import_edits);
    assert!(
        imported.starts_with("import java.util.List;"),
        "expected import insertion at start of file; got:\n{imported}"
    );

    let fqn_edit = fqn_action.edit.as_ref().expect("expected fqn edit");
    let fqn_changes = fqn_edit.changes.as_ref().expect("expected changes map");
    let fqn_edits = fqn_changes.get(&uri).expect("expected edits for file");
    let qualified = apply_lsp_edits(source, fqn_edits);
    assert!(
        qualified.contains("class A { java.util.List<String> xs; }"),
        "expected fully qualified type reference; got:\n{qualified}"
    );
}
