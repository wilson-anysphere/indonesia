use lsp_types::{CodeActionKind, Diagnostic, DiagnosticSeverity, NumberOrString, Range, Uri};
use nova_ide::code_action::diagnostic_quick_fixes;

use crate::text_fixture::offset_to_position;

#[test]
fn return_mismatch_diagnostic_quick_fixes_offer_remove_returned_value_for_void_methods() {
    let source = "class A { void m() { return 1; } }";
    let uri: Uri = "file:///test.java".parse().expect("valid uri");

    let expr_start = source.find("1").expect("expected `1` in fixture");
    let expr_end = expr_start + "1".len();

    let range = Range::new(
        offset_to_position(source, expr_start),
        offset_to_position(source, expr_end),
    );

    let selection = range.clone();
    let diagnostic = Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String("return-mismatch".to_string())),
        message: "cannot return a value from a `void` method".to_string(),
        ..Diagnostic::default()
    };

    let actions = diagnostic_quick_fixes(source, Some(uri.clone()), selection, &[diagnostic]);

    let remove = actions
        .iter()
        .find(|action| action.title == "Remove returned value")
        .expect("expected Remove returned value quickfix");
    assert_eq!(remove.kind, Some(CodeActionKind::QUICKFIX));
    let edit = remove.edit.as_ref().expect("expected workspace edit");
    let changes = edit.changes.as_ref().expect("expected changes");
    let edits = changes.get(&uri).expect("expected edits for uri");
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].new_text, "");
}

#[test]
fn return_mismatch_diagnostic_quick_fixes_offer_cast_to_expected_type() {
    let source = "class A { String m() { Object o = null; return o; } }";
    let uri: Uri = "file:///test.java".parse().expect("valid uri");

    let needle = "return o;";
    let stmt_start = source
        .find(needle)
        .expect("expected return statement in fixture");
    let expr_start = stmt_start + "return ".len();
    let expr_end = expr_start + "o".len();

    let range = Range::new(
        offset_to_position(source, expr_start),
        offset_to_position(source, expr_end),
    );

    let selection = range.clone();
    let diagnostic = Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String("return-mismatch".to_string())),
        message: "return type mismatch: expected String, found Object".to_string(),
        ..Diagnostic::default()
    };

    let actions = diagnostic_quick_fixes(source, Some(uri.clone()), selection, &[diagnostic]);

    let cast = actions
        .iter()
        .find(|action| action.title == "Cast to String")
        .expect("expected Cast to String quickfix");
    assert_eq!(cast.kind, Some(CodeActionKind::QUICKFIX));
    let edit = cast.edit.as_ref().expect("expected workspace edit");
    let changes = edit.changes.as_ref().expect("expected changes");
    let edits = changes.get(&uri).expect("expected edits for uri");
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].new_text, "(String) (o)");
}
