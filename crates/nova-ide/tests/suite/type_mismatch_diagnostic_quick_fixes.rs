use lsp_types::{CodeActionKind, Diagnostic, DiagnosticSeverity, NumberOrString, Range, Uri};
use nova_ide::code_action::diagnostic_quick_fixes;

use crate::text_fixture::offset_to_position;

#[test]
fn type_mismatch_diagnostic_quick_fixes_offer_cast_and_convert_to_string() {
    let source = r#"
class A {
  void m() {
    Object o = null;
    String s = o;
  }
}
"#;
    let uri: Uri = "file:///test.java".parse().expect("valid uri");

    let needle = "String s = o;";
    let stmt_start = source.find(needle).expect("expected assignment in fixture");
    let expr_start = stmt_start + "String s = ".len();
    let expr_end = expr_start + "o".len();

    let range = Range::new(
        offset_to_position(source, expr_start),
        offset_to_position(source, expr_end),
    );

    let selection = range.clone();
    let diagnostic = Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String("type-mismatch".to_string())),
        message: "type mismatch: expected String, found Object".to_string(),
        ..Diagnostic::default()
    };

    let actions = diagnostic_quick_fixes(source, Some(uri.clone()), selection, &[diagnostic]);

    let convert = actions
        .iter()
        .find(|action| action.title == "Convert to String")
        .expect("expected Convert to String quickfix");
    assert_eq!(convert.kind, Some(CodeActionKind::QUICKFIX));
    assert_eq!(convert.is_preferred, Some(true));
    let edit = convert.edit.as_ref().expect("expected workspace edit");
    let changes = edit.changes.as_ref().expect("expected changes");
    let edits = changes.get(&uri).expect("expected edits for uri");
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].new_text, "String.valueOf(o)");

    let cast = actions
        .iter()
        .find(|action| action.title == "Cast to String")
        .expect("expected Cast to String quickfix");
    assert_eq!(cast.kind, Some(CodeActionKind::QUICKFIX));
    assert_eq!(cast.is_preferred, Some(false));
    let edit = cast.edit.as_ref().expect("expected workspace edit");
    let changes = edit.changes.as_ref().expect("expected changes");
    let edits = changes.get(&uri).expect("expected edits for uri");
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].new_text, "(String) o");
}
