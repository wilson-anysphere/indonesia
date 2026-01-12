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
fn extract_variable_code_actions_offer_var_and_explicit_type_variants() {
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
    assert_eq!(actions.len(), 2);

    let mut var_action = None;
    let mut explicit_action = None;
    for action in actions.drain(..) {
        let lsp_types::CodeActionOrCommand::CodeAction(action) = action else {
            panic!("expected code action");
        };
        match action.title.as_str() {
            "Extract variable…" => var_action = Some(action),
            "Extract variable… (explicit type)" => explicit_action = Some(action),
            other => panic!("unexpected action title: {other}"),
        }
    }

    // `var` variant.
    let mut var_action = var_action.expect("var extract variable action");
    resolve_extract_variable_code_action(&uri, &source, &mut var_action, Some("sum".to_string()))
        .expect("resolve var action");

    let var_edit = var_action.edit.expect("resolved edit");
    let var_changes = var_edit.changes.expect("changes");
    let var_edits = var_changes.get(&uri).expect("edits for uri");

    let actual_var = apply_lsp_edits(&source, var_edits);
    let expected_var = r#"
class C {
    void m() {
        var sum = 1 + 2;
        int x = sum;
        System.out.println(x);
    }
}
"#;

    assert_eq!(actual_var, expected_var);

    // Explicit type variant.
    let mut explicit_action = explicit_action.expect("explicit type extract variable action");
    resolve_extract_variable_code_action(
        &uri,
        &source,
        &mut explicit_action,
        Some("sum".to_string()),
    )
    .expect("resolve explicit type action");

    let explicit_edit = explicit_action.edit.expect("resolved edit");
    let explicit_changes = explicit_edit.changes.expect("changes");
    let explicit_edits = explicit_changes.get(&uri).expect("edits for uri");

    let actual_explicit = apply_lsp_edits(&source, explicit_edits);
    let expected_explicit = r#"
class C {
    void m() {
        int sum = 1 + 2;
        int x = sum;
        System.out.println(x);
    }
}
"#;

    assert_eq!(actual_explicit, expected_explicit);
}

#[test]
fn extract_variable_code_actions_still_offered_when_default_name_conflicts() {
    let fixture = r#"
class C {
    void m() {
        int extracted = 0;
        int x = /*start*/1 + 2/*end*/;
        System.out.println(x + extracted);
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
    assert_eq!(actions.len(), 2);
    assert!(actions.iter().any(|action| match action {
        lsp_types::CodeActionOrCommand::CodeAction(action) => action.title == "Extract variable…",
        _ => false,
    }));
    assert!(actions.iter().any(|action| match action {
        lsp_types::CodeActionOrCommand::CodeAction(action) => {
            action.title == "Extract variable… (explicit type)"
        }
        _ => false,
    }));
}

#[test]
fn extract_variable_code_actions_only_offer_var_when_explicit_type_inference_fails() {
    // `x` is a name expression. With only `TextDatabase` available, the parser cannot infer an
    // explicit type for it, but `var` extraction is still applicable.
    let fixture = r#"
class C {
    void m(Object x) {
        System.out.println(/*start*/x/*end*/);
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
    assert_eq!(actions.len(), 1);

    let lsp_types::CodeActionOrCommand::CodeAction(action) = &actions[0] else {
        panic!("expected code action");
    };
    assert_eq!(action.title, "Extract variable…");
}

#[test]
fn extract_variable_code_actions_still_offered_when_placeholder_shadows_field() {
    let fixture = r#"
class C {
    int extracted = 0;

    void m() {
        int x = /*start*/1 + 2/*end*/;
        System.out.println(extracted);
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
    assert_eq!(actions.len(), 2);
    assert!(actions.iter().any(|action| match action {
        lsp_types::CodeActionOrCommand::CodeAction(action) => action.title == "Extract variable…",
        _ => false,
    }));
    assert!(actions.iter().any(|action| match action {
        lsp_types::CodeActionOrCommand::CodeAction(action) => {
            action.title == "Extract variable… (explicit type)"
        }
        _ => false,
    }));
}

#[test]
fn extract_variable_code_action_not_offered_for_side_effectful_expression() {
    let fixture = r#"
class Foo {}
class C {
    void m() {
        Foo x = /*start*/new Foo()/*end*/;
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
    assert!(
        actions.is_empty(),
        "expected extract variable to be unavailable for side-effectful expression, got: {actions:?}"
    );
}

#[test]
fn extract_variable_code_action_not_offered_for_instanceof_pattern_expression() {
    let fixture = r#"
class C {
    void m(Object obj) {
        if (/*start*/obj instanceof String s/*end*/ && s.length() > 0) {
            System.out.println(s);
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
fn extract_variable_code_action_not_offered_in_annotation_value_nested_expression() {
    let fixture = r#"
class C {
    @A(1 + /*start*/2/*end*/)
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
fn extract_variable_code_action_not_offered_in_annotation_default_value() {
    let fixture = r#"
@interface TestAnno {
    String value() default /*start*/"unchecked"/*end*/;
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
fn extract_variable_code_action_not_offered_in_expression_bodied_lambda() {
    let fixture = r#"
class C {
    void m() {
        Runnable r = () -> System.out.println(/*start*/1 + 2/*end*/);
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
fn extract_variable_code_action_not_offered_in_annotation_default_value_nested_expression() {
    let fixture = r#"
@interface TestAnno {
    int value() default 1 + /*start*/2/*end*/;
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
fn extract_variable_code_action_not_offered_in_switch_case_label_nested_expression() {
    let fixture = r#"
class C {
    void m(int x) {
        switch (x) {
            case 1 + /*start*/2/*end*/:
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
fn extract_variable_code_action_not_offered_in_switch_expression_case_label() {
    let fixture = r#"
class C {
    int m(int x) {
        return switch (x) {
            case /*start*/1 + 2/*end*/ -> 0;
            default -> 1;
        };
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
fn extract_variable_code_action_not_offered_in_switch_expression_case_label_nested_expression() {
    let fixture = r#"
class C {
    int m(int x) {
        return switch (x) {
            case 1 + /*start*/2/*end*/ -> 0;
            default -> 1;
        };
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
fn extract_variable_code_action_not_offered_in_switch_expression_rule_expression() {
    let fixture = r#"
class C {
    int m(int x) {
        return switch (x) {
            case 1 -> /*start*/1 + 2/*end*/;
            default -> 0;
        };
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
fn extract_variable_code_action_not_offered_in_explicit_constructor_invocation() {
    let fixture = r#"
class B {
    B(int x) {}
}

class C extends B {
    C() {
        super(/*start*/1 + 2/*end*/);
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
fn extract_variable_code_action_not_offered_in_this_constructor_invocation() {
    let fixture = r#"
class C {
    C(int x) {}

    C() {
        this(/*start*/1 + 2/*end*/);
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
fn extract_variable_code_action_not_offered_in_for_condition() {
    let fixture = r#"
class C {
    void m() {
        for (int i = 0; /*start*/i < 10/*end*/; i++) {
            System.out.println(i);
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
fn extract_variable_code_action_not_offered_in_for_update() {
    let fixture = r#"
class C {
    void m(int n, int step) {
        for (int i = 0; i < n; i += /*start*/step/*end*/) {
            System.out.println(i);
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
fn extract_variable_code_action_not_offered_for_if_body_without_braces_multiline() {
    let fixture = r#"
class C {
    void m(boolean cond) {
        if (cond)
            System.out.println(/*start*/1 + 2/*end*/);
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
fn extract_variable_code_action_not_offered_for_if_body_without_braces_oneline() {
    let fixture = r#"
class C {
    void m(boolean cond) {
        if (cond) System.out.println(/*start*/1 + 2/*end*/);
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
fn extract_variable_code_action_not_offered_for_while_body_without_braces_multiline() {
    let fixture = r#"
class C {
    void m(boolean cond) {
        while (cond)
            System.out.println(/*start*/1 + 2/*end*/);
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
fn extract_variable_code_action_not_offered_for_do_while_body_without_braces_multiline() {
    let fixture = r#"
class C {
    void m(boolean cond) {
        do
            System.out.println(/*start*/1 + 2/*end*/);
        while (cond);
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
fn extract_variable_code_action_not_offered_for_for_body_without_braces_multiline() {
    let fixture = r#"
class C {
    void m() {
        for (int i = 0; i < 10; i++)
            System.out.println(/*start*/1 + 2/*end*/);
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
fn extract_variable_code_action_not_offered_for_oneline_switch_case_statement() {
    let fixture = r#"
class C {
    void m(int x) {
        switch (x) { case 1: System.out.println(/*start*/1 + 2/*end*/); }
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
fn extract_variable_code_action_not_offered_for_switch_statement_rule_body_multiline() {
    let fixture = r#"
class C {
    void m(int x) {
        switch (x) {
            case 1 ->
                System.out.println(/*start*/1 + 2/*end*/);
            default -> {}
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
fn extract_variable_not_offered_inside_try_with_resources_resource_specification() {
    let fixture = r#"
class C {
    void m(java.io.InputStream in) throws Exception {
        try (java.io.BufferedInputStream r = new java.io.BufferedInputStream(/*start*/in/*end*/)) {
            r.read();
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
fn extract_variable_code_action_not_offered_in_try_with_resources_resource_initializer() {
    let fixture = r#"
class C {
    void m() throws Exception {
        try (java.io.ByteArrayInputStream r = new java.io.ByteArrayInputStream(new byte[/*start*/1 + 2/*end*/])) {
            r.read();
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
fn extract_variable_code_action_not_offered_for_switch_rule_statement_body_without_braces_multiline(
) {
    let fixture = r#"
class C {
    void m(int x) {
        switch (x) {
            case 1 ->
                System.out.println(/*start*/1 + 2/*end*/);
            default -> {
                System.out.println(0);
            }
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
fn extract_variable_code_action_not_offered_for_labeled_statement_body_without_braces() {
    let fixture = r#"
class C {
    void m() {
        label:
            System.out.println(/*start*/1 + 2/*end*/);
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
fn extract_variable_code_action_not_offered_in_arrow_switch_rule_expression_body() {
    let fixture = r#"
class C {
    int m(int x) {
        return switch (x) {
            case 1 -> /*start*/1 + 2/*end*/;
            default -> 0;
        };
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
            lsp_types::CodeActionOrCommand::CodeAction(action)
                if action.title == "Inline variable" =>
            {
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
fn inline_variable_code_actions_when_cursor_on_declaration_only_offers_inline_all_usages() {
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
    let offset = source.find("int a = 1 + 2;").expect("declaration exists") + "int ".len();
    let position = offset_to_position(source, offset);

    let actions = inline_variable_code_actions(&uri, source, position);

    let mut titles = Vec::new();
    for action in &actions {
        let lsp_types::CodeActionOrCommand::CodeAction(action) = action else {
            panic!("expected code action");
        };
        titles.push(action.title.clone());
    }

    assert_eq!(actions.len(), 1);
    assert_eq!(titles, vec!["Inline variable (all usages)".to_string()]);

    let lsp_types::CodeActionOrCommand::CodeAction(action) = &actions[0] else {
        panic!("expected code action");
    };
    assert!(action.disabled.is_none(), "expected action enabled");
}

#[test]
fn inline_variable_does_not_touch_shadowed_identifiers() {
    let source = r#"
class C {
    void m() {
        int a = 1 + 2;
        {
            int a = 5;
            System.out.println(a);
        }
        System.out.println(a);
    }
}
"#;

    let uri = Uri::from_str("file:///Test.java").unwrap();
    let outer_offset = source.rfind("println(a)").expect("outer println call") + "println(".len();
    let position = offset_to_position(source, outer_offset);

    let actions = inline_variable_code_actions(&uri, source, position);
    assert_eq!(actions.len(), 2);

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
        {
            int a = 5;
            System.out.println(a);
        }
        System.out.println((1 + 2));
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn inline_variable_code_actions_apply_expected_edits_in_switch_case_label() {
    let source = r#"
class C {
    void m(int x) {
        switch (x) {
            case 1: int a = 1 + 2; System.out.println(a); break;
        }
    }
}
"#;

    let uri = Uri::from_str("file:///Test.java").unwrap();
    let offset = source.find("println(a)").expect("println call") + "println(".len();
    let position = offset_to_position(source, offset);

    let actions = inline_variable_code_actions(&uri, source, position);
    assert_eq!(actions.len(), 2);

    // Apply the "all usages" variant (should delete the declaration but preserve `case 1:`).
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
    void m(int x) {
        switch (x) {
            case 1: System.out.println((1 + 2)); break;
        }
    }
}
"#;

    assert_eq!(actual, expected);
}

#[test]
fn inline_variable_inline_all_not_offered_when_unindexed_usage_exists() {
    let source = r#"
class C {
    void m() {
        int a = 1 + 2;
        Runnable r = new Runnable() { public void run() { System.out.println(a); } };
        System.out.println(a);
    }
}
"#;

    let uri = Uri::from_str("file:///Test.java").unwrap();
    // Use the non-anonymous-class usage; the usage inside the anonymous class body is intentionally
    // unindexed and would not resolve to a symbol at the cursor.
    let offset = source.rfind("println(a);").expect("println call") + "println(".len();
    let position = offset_to_position(source, offset);

    let actions = inline_variable_code_actions(&uri, source, position);
    assert_eq!(actions.len(), 1, "inline-all must not be offered");

    let single = actions
        .into_iter()
        .find_map(|action| match action {
            lsp_types::CodeActionOrCommand::CodeAction(action)
                if action.title == "Inline variable" =>
            {
                Some(action)
            }
            _ => None,
        })
        .expect("inline variable action");

    assert!(
        single.disabled.is_none(),
        "single-usage inline should still be supported"
    );
    let edit = single.edit.expect("edit");
    let changes = edit.changes.expect("changes");
    let edits = changes.get(&uri).expect("edits for uri");
    let actual = apply_lsp_edits(source, edits);

    assert!(
        actual.contains("int a = 1 + 2;"),
        "declaration must remain (unknown usage exists)"
    );
    assert!(
        actual.contains("System.out.println((1 + 2));"),
        "selected usage should be inlined: {actual}"
    );
}

#[test]
fn inline_variable_inline_all_not_offered_when_unindexed_qualified_usage_exists() {
    let source = r#"
class C {
    void m() {
        String a = "hi";
        Runnable r = new Runnable() { public void run() { System.out.println(a.length()); } };
        System.out.println(a);
    }
}
"#;

    let uri = Uri::from_str("file:///Test.java").unwrap();
    // Place the cursor on the indexed (non-anonymous-class) usage; the usage inside the anonymous
    // class body is intentionally unindexed.
    let offset = source.find("println(a);").expect("println call") + "println(".len();
    let position = offset_to_position(source, offset);

    let actions = inline_variable_code_actions(&uri, source, position);
    assert_eq!(actions.len(), 1, "inline-all must not be offered");

    let single = actions
        .into_iter()
        .find_map(|action| match action {
            lsp_types::CodeActionOrCommand::CodeAction(action)
                if action.title == "Inline variable" =>
            {
                Some(action)
            }
            _ => None,
        })
        .expect("inline variable action");

    let edit = single.edit.expect("edit");
    let changes = edit.changes.expect("changes");
    let edits = changes.get(&uri).expect("edits for uri");
    let actual = apply_lsp_edits(source, edits);

    assert!(
        actual.contains("String a = \"hi\";"),
        "declaration must remain (unknown usage exists)"
    );
    assert!(
        actual.contains("System.out.println(\"hi\");"),
        "selected usage should be inlined: {actual}"
    );
    assert!(
        actual.contains("System.out.println(a.length());"),
        "unindexed usage must remain untouched: {actual}"
    );
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

#[test]
fn extract_variable_not_offered_inside_assert_statement() {
    let fixture = r#"
class C {
    void m(int x) {
        assert /*start*/x > 0/*end*/;
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
