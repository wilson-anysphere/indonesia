use lsp_types::{CodeActionKind, Diagnostic, DiagnosticSeverity, NumberOrString, Range, Uri};
use nova_ide::code_action::diagnostic_quick_fixes;

use crate::text_fixture::{offset_to_position, position_to_offset};

fn apply_lsp_text_edits(source: &str, edits: &[lsp_types::TextEdit]) -> String {
    let mut edits_with_offsets: Vec<(usize, usize, String)> = edits
        .iter()
        .map(|edit| {
            let start = position_to_offset(source, edit.range.start).expect("valid start pos");
            let end = position_to_offset(source, edit.range.end).expect("valid end pos");
            (start, end, edit.new_text.clone())
        })
        .collect();

    // Apply from back to front.
    edits_with_offsets.sort_by(|(a_start, _, _), (b_start, _, _)| b_start.cmp(a_start));

    let mut out = source.to_string();
    for (start, end, new_text) in edits_with_offsets {
        out.replace_range(start..end, &new_text);
    }
    out
}

#[test]
fn unresolved_method_offers_create_method_quick_fix() {
    let source = r#"class A {
  void m() {
    foo();
  }
}
"#;
    let uri: Uri = "file:///test.java".parse().expect("valid uri");

    let foo_start = source.find("foo").expect("expected `foo` in fixture");
    let foo_end = foo_start + "foo".len();
    let range = Range::new(
        offset_to_position(source, foo_start),
        offset_to_position(source, foo_end),
    );

    let diagnostic = Diagnostic {
        range: range.clone(),
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String("unresolved-method".to_string())),
        message: "unresolved method `foo`".to_string(),
        ..Diagnostic::default()
    };

    let actions = diagnostic_quick_fixes(source, Some(uri.clone()), range, &[diagnostic]);
    let action = actions
        .iter()
        .find(|action| action.title == "Create method 'foo'")
        .expect("expected Create method quick fix");

    assert_eq!(action.kind, Some(CodeActionKind::QUICKFIX));

    let edit = action.edit.as_ref().expect("expected workspace edit");
    let changes = edit.changes.as_ref().expect("expected changes");
    let edits = changes.get(&uri).expect("expected edit for file");
    assert_eq!(edits.len(), 1);

    let last_brace = source.rfind('}').expect("expected closing brace");
    let expected_pos = offset_to_position(source, last_brace);
    assert_eq!(edits[0].range.start, expected_pos);
    assert_eq!(edits[0].range.end, expected_pos);
    assert!(
        edits[0]
            .new_text
            .contains("private Object foo(Object... args)"),
        "expected method stub; got {:?}",
        edits[0].new_text
    );

    let updated = apply_lsp_text_edits(source, edits);
    let stub_idx = updated
        .find("private Object foo(Object... args)")
        .expect("stub insertion");
    let final_brace = updated.rfind('}').expect("updated closing brace");
    assert!(stub_idx < final_brace, "expected stub before final brace");
}

#[test]
fn unresolved_field_offers_create_field_quick_fix() {
    let source = r#"class A {
  void m() {
    System.out.println(this.bar);
  }
}
"#;
    let uri: Uri = "file:///test.java".parse().expect("valid uri");

    let bar_start = source.find("bar").expect("expected `bar` in fixture");
    let bar_end = bar_start + "bar".len();
    let range = Range::new(
        offset_to_position(source, bar_start),
        offset_to_position(source, bar_end),
    );

    let diagnostic = Diagnostic {
        range: range.clone(),
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String("unresolved-field".to_string())),
        message: "unresolved field `bar`".to_string(),
        ..Diagnostic::default()
    };

    let actions = diagnostic_quick_fixes(source, Some(uri.clone()), range, &[diagnostic]);
    let action = actions
        .iter()
        .find(|action| action.title == "Create field 'bar'")
        .expect("expected Create field quick fix");

    assert_eq!(action.kind, Some(CodeActionKind::QUICKFIX));

    let edit = action.edit.as_ref().expect("expected workspace edit");
    let changes = edit.changes.as_ref().expect("expected changes");
    let edits = changes.get(&uri).expect("expected edit for file");
    assert_eq!(edits.len(), 1);

    let last_brace = source.rfind('}').expect("expected closing brace");
    let expected_pos = offset_to_position(source, last_brace);
    assert_eq!(edits[0].range.start, expected_pos);
    assert_eq!(edits[0].range.end, expected_pos);
    assert!(
        edits[0].new_text.contains("private Object bar;"),
        "expected field stub; got {:?}",
        edits[0].new_text
    );

    let updated = apply_lsp_text_edits(source, edits);
    let stub_idx = updated.find("private Object bar;").expect("stub insertion");
    let final_brace = updated.rfind('}').expect("updated closing brace");
    assert!(stub_idx < final_brace, "expected stub before final brace");
}
