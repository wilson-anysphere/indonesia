use lsp_types::Uri;
use nova_ide::code_action::ExtractMethodCommandArgs;
use nova_lsp::extract_method;
use nova_refactor::extract_method::{InsertionStrategy, Visibility};
use nova_test_utils::{apply_lsp_edits, extract_range, offset_to_position};
use std::str::FromStr;

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
    let command = action
        .command
        .expect("action should be represented as a command");
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
