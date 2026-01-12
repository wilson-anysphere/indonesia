use lsp_types::Uri;
use nova_ide::code_action::ExtractMethodCommandArgs;
use nova_lsp::extract_method;
use nova_refactor::extract_method::{InsertionStrategy, Visibility};
use nova_test_utils::extract_range;
use std::str::FromStr;

fn offset_to_position(text: &str, offset: usize) -> lsp_types::Position {
    let mut line: u32 = 0;
    let mut col_utf16: u32 = 0;
    let mut idx = 0;

    for ch in text.chars() {
        if idx >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            col_utf16 = 0;
        } else {
            col_utf16 += ch.len_utf16() as u32;
        }
        idx += ch.len_utf8();
    }

    lsp_types::Position {
        line,
        character: col_utf16,
    }
}

fn position_to_offset(text: &str, pos: lsp_types::Position) -> Option<usize> {
    let mut line: u32 = 0;
    let mut col_utf16: u32 = 0;
    let mut idx = 0;

    for ch in text.chars() {
        if line == pos.line && col_utf16 == pos.character {
            return Some(idx);
        }

        if ch == '\n' {
            if line == pos.line {
                if col_utf16 == pos.character {
                    return Some(idx);
                }
                return None;
            }
            line += 1;
            col_utf16 = 0;
            idx += 1;
            continue;
        }

        if line == pos.line {
            col_utf16 += ch.len_utf16() as u32;
            if col_utf16 > pos.character {
                return None;
            }
        }
        idx += ch.len_utf8();
    }

    if line == pos.line && col_utf16 == pos.character {
        Some(idx)
    } else {
        None
    }
}

fn apply_lsp_edits(source: &str, edits: &[lsp_types::TextEdit]) -> String {
    let mut byte_edits: Vec<(usize, usize, &str)> = edits
        .iter()
        .map(|e| {
            let start = position_to_offset(source, e.range.start).expect("start offset");
            let end = position_to_offset(source, e.range.end).expect("end offset");
            (start, end, e.new_text.as_str())
        })
        .collect();

    // Apply from the end so offsets remain stable.
    byte_edits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));

    // Overlap check.
    let mut last_start = source.len();
    for (start, end, _) in &byte_edits {
        assert!(*start <= *end);
        assert!(*end <= source.len());
        assert!(*end <= last_start, "overlapping edits");
        last_start = *start;
    }

    let mut out = source.to_string();
    for (start, end, text) in byte_edits {
        out.replace_range(start..end, text);
    }
    out
}

#[test]
fn lsp_execute_extract_method_produces_workspace_edit() {
    let fixture = r#"
class C {
    void m(int a) {
        int b = 1;
        /*start*/System.out.println(a + b);/*end*/
        System.out.println("done");
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let uri = Uri::from_str("file:///test.java").unwrap();

    let args = ExtractMethodCommandArgs {
        uri: uri.clone(),
        range: lsp_types::Range {
            start: offset_to_position(&source, selection.start),
            end: offset_to_position(&source, selection.end),
        },
        name: "extracted".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let edit = extract_method::execute(&source, args).expect("command should succeed");
    // Ensure the edit round-trips through JSON serialization (as it would over LSP).
    let edit: lsp_types::WorkspaceEdit =
        serde_json::from_value(serde_json::to_value(&edit).expect("serialize workspace edit"))
            .expect("deserialize workspace edit");
    let changes = edit.changes.expect("workspace edit should have changes");
    let edits = changes.get(&uri).expect("edits for uri must exist");

    let actual = apply_lsp_edits(&source, edits);
    let expected = r#"
class C {
    void m(int a) {
        int b = 1;
        extracted(a, b);
        System.out.println("done");
    }

    private void extracted(int a, int b) {
        System.out.println(a + b);
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn lsp_code_action_is_offered_for_extractable_region() {
    let fixture = r#"
class C {
    void m(int a) {
        int b = 1;
        /*start*/System.out.println(a + b);/*end*/
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let uri = Uri::from_str("file:///test.java").unwrap();
    let range = lsp_types::Range {
        start: offset_to_position(&source, selection.start),
        end: offset_to_position(&source, selection.end),
    };

    let action = extract_method::code_action(&source, uri.clone(), range.clone());
    let action = action.expect("extract method action");
    let command = action.command.expect("action should be represented as a command");
    assert_eq!(command.command, "nova.extractMethod");
    let args_value = command
        .arguments
        .and_then(|args| args.into_iter().next())
        .expect("extract method args");
    let decoded: ExtractMethodCommandArgs =
        serde_json::from_value(args_value).expect("decode extract method args");
    assert_eq!(decoded.uri, uri);
    assert_eq!(decoded.range, range);
}

#[test]
fn lsp_execute_extract_method_multi_statement_produces_workspace_edit() {
    let fixture = r#"
class C {
    void m(int a) {
        int b = 1;
        /*start*/System.out.println(a + b);
        System.out.println("done");/*end*/
        System.out.println("after");
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let uri = Uri::from_str("file:///test.java").unwrap();

    let args = ExtractMethodCommandArgs {
        uri: uri.clone(),
        range: lsp_types::Range {
            start: offset_to_position(&source, selection.start),
            end: offset_to_position(&source, selection.end),
        },
        name: "extracted".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let edit = extract_method::execute(&source, args).expect("command should succeed");
    let edit: lsp_types::WorkspaceEdit =
        serde_json::from_value(serde_json::to_value(&edit).expect("serialize workspace edit"))
            .expect("deserialize workspace edit");
    let changes = edit.changes.expect("workspace edit should have changes");
    let edits = changes.get(&uri).expect("edits for uri must exist");

    let actual = apply_lsp_edits(&source, edits);
    let expected = r#"
class C {
    void m(int a) {
        int b = 1;
        extracted(a, b);
        System.out.println("after");
    }

    private void extracted(int a, int b) {
        System.out.println(a + b);
        System.out.println("done");
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn lsp_execute_extract_method_expression_produces_workspace_edit() {
    let fixture = r#"
class C {
    int m(int a, int b) {
        return /*start*/a + b/*end*/;
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let uri = Uri::from_str("file:///test.java").unwrap();

    let args = ExtractMethodCommandArgs {
        uri: uri.clone(),
        range: lsp_types::Range {
            start: offset_to_position(&source, selection.start),
            end: offset_to_position(&source, selection.end),
        },
        name: "sum".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let edit = extract_method::execute(&source, args).expect("command should succeed");
    let edit: lsp_types::WorkspaceEdit =
        serde_json::from_value(serde_json::to_value(&edit).expect("serialize workspace edit"))
            .expect("deserialize workspace edit");
    let changes = edit.changes.expect("workspace edit should have changes");
    let edits = changes.get(&uri).expect("edits for uri must exist");

    let actual = apply_lsp_edits(&source, edits);
    let expected = r#"
class C {
    int m(int a, int b) {
        return sum(a, b);
    }

    private int sum(int a, int b) {
        return a + b;
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn lsp_code_action_is_offered_for_multi_statement_selection() {
    let fixture = r#"
class C {
    void m(int a) {
        int b = 1;
        /*start*/System.out.println(a + b);
        System.out.println("done");/*end*/
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let uri = Uri::from_str("file:///test.java").unwrap();
    let range = lsp_types::Range {
        start: offset_to_position(&source, selection.start),
        end: offset_to_position(&source, selection.end),
    };

    let action = extract_method::code_action(&source, uri, range);
    assert!(action.is_some());
}

#[test]
fn lsp_code_action_is_offered_for_expression_selection() {
    let fixture = r#"
class C {
    int m(int a, int b) {
        return /*start*/a + b/*end*/;
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let uri = Uri::from_str("file:///test.java").unwrap();
    let range = lsp_types::Range {
        start: offset_to_position(&source, selection.start),
        end: offset_to_position(&source, selection.end),
    };

    let action = extract_method::code_action(&source, uri, range);
    assert!(action.is_some());
}
