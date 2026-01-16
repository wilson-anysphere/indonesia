use crate::framework_harness::offset_to_position;
use lsp_types::{CodeActionKind, DiagnosticSeverity, NumberOrString, Range};
use nova_ide::code_action::diagnostic_quick_fixes;
use nova_refactor::position_to_offset_utf16;
use nova_types::{Diagnostic as NovaDiagnostic, Span};

fn second_import_offset(source: &str) -> usize {
    let mut matches = source.match_indices("import a.Foo;");
    matches.next().expect("expected first import");
    matches.next().expect("expected second import in fixture").0
}

#[test]
fn diagnostic_quick_fixes_includes_remove_duplicate_import() {
    let source = "import a.Foo;\nimport a.Foo;\nclass A {}\n";
    let uri: lsp_types::Uri = "file:///test.java".parse().expect("valid uri");

    let start = second_import_offset(source);
    let end = start + "import a.Foo;".len();
    let range = Range::new(
        offset_to_position(source, start),
        offset_to_position(source, end),
    );
    let diag = lsp_types::Diagnostic {
        range: range.clone(),
        severity: Some(DiagnosticSeverity::WARNING),
        code: Some(NumberOrString::String("duplicate-import".to_string())),
        message: "duplicate import".to_string(),
        ..lsp_types::Diagnostic::default()
    };

    let actions = diagnostic_quick_fixes(source, Some(uri.clone()), range.clone(), &[diag]);
    let action = actions
        .iter()
        .find(|action| action.title == "Remove duplicate import")
        .expect("expected Remove duplicate import code action");

    assert_eq!(action.kind, Some(CodeActionKind::QUICKFIX));
    let edit = action.edit.as_ref().expect("expected workspace edit");
    let changes = edit.changes.as_ref().expect("expected changes-based edit");
    let edits = changes.get(&uri).expect("expected edits for file uri");
    assert_eq!(edits.len(), 1);
    let text_edit = &edits[0];
    assert!(text_edit.new_text.is_empty());

    let start = position_to_offset_utf16(source, text_edit.range.start).expect("start offset");
    let end = position_to_offset_utf16(source, text_edit.range.end).expect("end offset");
    let mut updated = source.to_string();
    updated.replace_range(start..end, &text_edit.new_text);

    assert_eq!(updated, "import a.Foo;\nclass A {}\n");
}

#[test]
fn quick_fixes_for_diagnostics_includes_remove_duplicate_import() {
    let source = "import a.Foo;\nimport a.Foo;\nclass A {}\n";
    let uri: lsp_types::Uri = "file:///test.java".parse().expect("valid uri");

    let start = second_import_offset(source);
    let end = start + "import a.Foo;".len();
    let span = Span::new(start, end);

    let diagnostics = vec![NovaDiagnostic::warning(
        "duplicate-import",
        "duplicate import",
        Some(span),
    )];

    let actions = nova_ide::__quick_fixes_for_diagnostics(&uri, source, span, &diagnostics);
    let action = actions
        .iter()
        .find_map(|action| match action {
            lsp_types::CodeActionOrCommand::CodeAction(action)
                if action.title == "Remove duplicate import" =>
            {
                Some(action)
            }
            _ => None,
        })
        .expect("expected Remove duplicate import code action");

    assert_eq!(action.kind, Some(CodeActionKind::QUICKFIX));
    let edit = action.edit.as_ref().expect("expected workspace edit");
    let changes = edit.changes.as_ref().expect("expected changes-based edit");
    let edits = changes.get(&uri).expect("expected edits for file uri");
    assert_eq!(edits.len(), 1);
    let text_edit = &edits[0];
    assert!(text_edit.new_text.is_empty());

    let start = position_to_offset_utf16(source, text_edit.range.start).expect("start offset");
    let end = position_to_offset_utf16(source, text_edit.range.end).expect("end offset");
    let mut updated = source.to_string();
    updated.replace_range(start..end, &text_edit.new_text);

    assert_eq!(updated, "import a.Foo;\nclass A {}\n");
}
