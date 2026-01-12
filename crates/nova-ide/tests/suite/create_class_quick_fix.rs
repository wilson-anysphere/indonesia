use lsp_types::{
    CodeActionKind, Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range, Uri,
};
use nova_ide::code_action::diagnostic_quick_fixes;

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
