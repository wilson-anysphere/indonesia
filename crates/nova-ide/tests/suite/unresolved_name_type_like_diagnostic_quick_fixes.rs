use lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Range, Uri};
use nova_ide::code_action::diagnostic_quick_fixes;

use crate::framework_harness::offset_to_position;

#[test]
fn unresolved_name_type_like_offers_import_and_qualify_quick_fixes() {
    let source = r#"class A { void m() { List.of("x"); } }"#;
    let uri: Uri = "file:///test.java".parse().expect("valid uri");

    let list_start = source.find("List").expect("expected `List` in fixture");
    let list_end = list_start + "List".len();
    let range = Range::new(
        offset_to_position(source, list_start),
        offset_to_position(source, list_end),
    );

    let diagnostic = Diagnostic {
        range: range.clone(),
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String("unresolved-name".to_string())),
        message: "unresolved reference `List`".to_string(),
        ..Diagnostic::default()
    };

    let actions = diagnostic_quick_fixes(source, Some(uri), range, &[diagnostic]);

    assert!(
        actions.iter().any(|action| action.title == "Import java.util.List"),
        "expected `Import java.util.List`; got {actions:#?}"
    );
    assert!(
        actions
            .iter()
            .any(|action| action.title == "Use fully qualified name 'java.util.List'"),
        "expected `Use fully qualified name 'java.util.List'`; got {actions:#?}"
    );
}

