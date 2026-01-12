use lsp_types::Uri;
use nova_core::{LineIndex, Position as CorePosition, TextSize};
use nova_lsp::refactor::{
    extract_variable_code_actions, inline_variable_code_actions,
    resolve_extract_variable_code_action,
};
use nova_test_utils::extract_range;
use pretty_assertions::assert_eq;
use std::str::FromStr;

fn offset_to_position(text: &str, offset: usize) -> lsp_types::Position {
    let index = LineIndex::new(text);
    let pos = index.position(text, TextSize::from(offset as u32));
    lsp_types::Position::new(pos.line, pos.character)
}

fn position_to_offset(text: &str, pos: lsp_types::Position) -> Option<usize> {
    let index = LineIndex::new(text);
    let pos = CorePosition::new(pos.line, pos.character);
    index
        .offset_of_position(text, pos)
        .map(|o| u32::from(o) as usize)
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
fn extract_variable_code_action_resolves_and_applies() {
    let fixture = r#"
class C {
    void m() {
        int x = /*start*/1 + 2/*end*/;
        System.out.println(x);
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let uri = Uri::from_str("file:///Test.java").unwrap();
    let range = lsp_types::Range {
        start: offset_to_position(&source, selection.start),
        end: offset_to_position(&source, selection.end),
    };

    let mut actions = extract_variable_code_actions(&uri, &source, range);
    assert_eq!(actions.len(), 1);

    let mut action = match actions.pop().unwrap() {
        lsp_types::CodeActionOrCommand::CodeAction(action) => action,
        _ => panic!("expected code action"),
    };

    resolve_extract_variable_code_action(&uri, &source, &mut action, Some("sum".to_string()))
        .expect("resolve");

    let edit = action.edit.expect("resolved edit");
    let changes = edit.changes.expect("changes");
    let edits = changes.get(&uri).expect("edits for uri");

    let actual = apply_lsp_edits(&source, edits);
    let expected = r#"
class C {
    void m() {
        var sum = 1 + 2;
        int x = sum;
        System.out.println(x);
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn extract_variable_code_action_not_offered_for_side_effectful_expression() {
    let fixture = r#"
class C {
    void m() {
        int x = /*start*/foo()/*end*/;
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let uri = Uri::from_str("file:///Test.java").unwrap();
    let range = lsp_types::Range {
        start: offset_to_position(&source, selection.start),
        end: offset_to_position(&source, selection.end),
    };

    let actions = extract_variable_code_actions(&uri, &source, range);
    assert!(actions.is_empty());
}

#[test]
fn extract_variable_code_action_not_offered_in_annotation_value() {
    let fixture = r#"
class C {
    @SuppressWarnings(/*start*/"unchecked"/*end*/)
    void m() {
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let uri = Uri::from_str("file:///Test.java").unwrap();
    let range = lsp_types::Range {
        start: offset_to_position(&source, selection.start),
        end: offset_to_position(&source, selection.end),
    };

    let actions = extract_variable_code_actions(&uri, &source, range);
    assert!(actions.is_empty());
}

#[test]
fn extract_variable_code_action_not_offered_in_switch_case_label() {
    let fixture = r#"
class C {
    void m(int x) {
        switch (x) {
            case /*start*/1 + 2/*end*/:
                break;
        }
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    let uri = Uri::from_str("file:///Test.java").unwrap();
    let range = lsp_types::Range {
        start: offset_to_position(&source, selection.start),
        end: offset_to_position(&source, selection.end),
    };

    let actions = extract_variable_code_actions(&uri, &source, range);
    assert!(actions.is_empty());
}

#[test]
fn inline_variable_code_actions_apply_expected_edits() {
    let source = r#"
class C {
    void m() {
        int a = 1 + 2;
        System.out.println(a);
        System.out.println(a);
    }
}
"#;

    let uri = Uri::from_str("file:///Test.java").unwrap();
    let offset = source.find("println(a)").expect("println call") + "println(".len();
    let position = offset_to_position(source, offset);

    let actions = inline_variable_code_actions(&uri, source, position);
    assert_eq!(actions.len(), 2);

    // Apply the single-usage variant (should keep the declaration).
    let single = actions
        .iter()
        .find_map(|action| match action {
            lsp_types::CodeActionOrCommand::CodeAction(action) if action.title == "Inline variable" => {
                Some(action.clone())
            }
            _ => None,
        })
        .expect("inline variable action");

    let edit = single.edit.expect("edit");
    let changes = edit.changes.expect("changes");
    let edits = changes.get(&uri).expect("edits for uri");
    let actual = apply_lsp_edits(source, edits);

    let expected = r#"
class C {
    void m() {
        int a = 1 + 2;
        System.out.println((1 + 2));
        System.out.println(a);
    }
}
"#;

    assert_eq!(actual, expected);

    // Apply the "all usages" variant (should delete the declaration).
    let all = actions
        .into_iter()
        .find_map(|action| match action {
            lsp_types::CodeActionOrCommand::CodeAction(action)
                if action.title == "Inline variable (all usages)" =>
            {
                Some(action)
            }
            _ => None,
        })
        .expect("inline all usages action");

    let edit = all.edit.expect("edit");
    let changes = edit.changes.expect("changes");
    let edits = changes.get(&uri).expect("edits for uri");
    let actual = apply_lsp_edits(source, edits);

    let expected = r#"
class C {
    void m() {
        System.out.println((1 + 2));
        System.out.println((1 + 2));
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn inline_variable_not_offered_outside_local_identifier() {
    let source = r#"
class C {
    void m() {
        int a = 1 + 2;
        System.out.println(a);
    }
}
"#;
    let uri = Uri::from_str("file:///Test.java").unwrap();

    // Cursor on the method name `m`.
    let offset = source.find("m()").unwrap();
    let position = offset_to_position(source, offset);

    let actions = inline_variable_code_actions(&uri, source, position);
    assert!(actions.is_empty());
}
