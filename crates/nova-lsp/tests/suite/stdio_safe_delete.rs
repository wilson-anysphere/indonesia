use lsp_types::{
    ApplyWorkspaceEditResponse, CodeActionContext, CodeActionParams, ExecuteCommandParams,
    Position, Range, TextDocumentIdentifier, TextEdit, Uri, WorkspaceEdit,
};
use pretty_assertions::assert_eq;
use serde_json::Value;
use std::io::BufReader;
use std::process::{Command, Stdio};

use crate::support::{
    did_open_notification, drain_notifications_until_id, exit_notification,
    initialize_request_empty, initialized_notification, jsonrpc_request, read_response_with_id,
    shutdown_request, write_jsonrpc_message,
};

fn apply_lsp_edits(original: &str, edits: &[TextEdit]) -> String {
    if edits.is_empty() {
        return original.to_string();
    }

    let index = nova_core::LineIndex::new(original);
    let core_edits: Vec<nova_core::TextEdit> = edits
        .iter()
        .map(|edit| {
            let range = nova_core::Range::new(
                nova_core::Position::new(edit.range.start.line, edit.range.start.character),
                nova_core::Position::new(edit.range.end.line, edit.range.end.character),
            );
            let range = index.text_range(original, range).expect("valid range");
            nova_core::TextEdit::new(range, edit.new_text.clone())
        })
        .collect();

    nova_core::apply_text_edits(original, &core_edits).expect("apply edits")
}

#[test]
fn stdio_server_supports_safe_delete_preview_then_apply() {
    let _lock = crate::support::stdio_server_lock();
    let fixture = r#"
 class A {
      public void used() {
    }

    public void entry() {
        if ("𝄞".isEmpty() && used()) {
        }
    }
}
"#;

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    // 1) initialize
    write_jsonrpc_message(&mut stdin, &initialize_request_empty(1));
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &initialized_notification());

    // 2) open document (required for safe delete symbol IDs to be stable in the stdio server)
    let uri: Uri = "file:///test/A.java".parse().unwrap();
    write_jsonrpc_message(
        &mut stdin,
        &did_open_notification(uri.clone(), "java", 1, fixture),
    );

    // 3) request code actions at the `used` method declaration
    let decl_offset = fixture
        .find("void used")
        .expect("method decl")
        .saturating_add("void ".len());
    let line_index = nova_core::LineIndex::new(fixture);
    let decl_pos = line_index.position(
        fixture,
        nova_core::TextSize::from(u32::try_from(decl_offset).expect("u32 offset")),
    );

    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            CodeActionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                range: Range::new(
                    Position::new(decl_pos.line, decl_pos.character),
                    Position::new(decl_pos.line, decl_pos.character),
                ),
                context: CodeActionContext::default(),
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            },
            2,
            "textDocument/codeAction",
        ),
    );

    let code_action_resp = read_response_with_id(&mut stdout, 2);
    let actions = code_action_resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("code actions array");
    let safe_delete_action = actions
        .iter()
        .find(|action| {
            action
                .get("title")
                .and_then(|v| v.as_str())
                .is_some_and(|t| t.starts_with("Safe delete"))
        })
        .expect("safe delete action");

    assert_eq!(
        safe_delete_action
            .pointer("/data/type")
            .and_then(|v| v.as_str()),
        Some("nova/refactor/preview")
    );
    assert_eq!(
        safe_delete_action
            .pointer("/command/command")
            .and_then(|v| v.as_str()),
        Some("nova.safeDelete")
    );
    assert_eq!(
        safe_delete_action
            .pointer("/data/report/target/name")
            .and_then(|v| v.as_str()),
        Some("used")
    );
    let target_id = safe_delete_action
        .pointer("/data/report/target/id")
        .and_then(|v| v.as_u64())
        .expect("target symbol id");

    // 4) request preview via executeCommand (the code action is wired to this command)
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            ExecuteCommandParams {
                command: "nova.safeDelete".to_string(),
                arguments: vec![Value::Object({
                    let mut args = serde_json::Map::new();
                    args.insert(
                        "target".to_string(),
                        Value::Number(serde_json::Number::from(target_id)),
                    );
                    args.insert("mode".to_string(), Value::String("safe".to_string()));
                    args
                })],
                work_done_progress_params: Default::default(),
            },
            3,
            "workspace/executeCommand",
        ),
    );
    let preview_via_command = read_response_with_id(&mut stdout, 3);
    assert_eq!(
        preview_via_command
            .pointer("/result/type")
            .and_then(|v| v.as_str()),
        Some("nova/refactor/preview")
    );

    // 5) request preview via custom method
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            Value::Object({
                let mut params = serde_json::Map::new();
                params.insert(
                    "target".to_string(),
                    Value::Number(serde_json::Number::from(target_id)),
                );
                params.insert("mode".to_string(), Value::String("safe".to_string()));
                params
            }),
            4,
            "nova/refactor/safeDelete",
        ),
    );
    let preview_via_method = read_response_with_id(&mut stdout, 4);
    assert_eq!(
        preview_via_method
            .pointer("/result/type")
            .and_then(|v| v.as_str()),
        Some("nova/refactor/preview")
    );

    // 6) apply via executeCommand
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            ExecuteCommandParams {
                command: "nova.safeDelete".to_string(),
                arguments: vec![Value::Object({
                    let mut args = serde_json::Map::new();
                    args.insert(
                        "target".to_string(),
                        Value::Number(serde_json::Number::from(target_id)),
                    );
                    args.insert(
                        "mode".to_string(),
                        Value::String("deleteAnyway".to_string()),
                    );
                    args
                })],
                work_done_progress_params: Default::default(),
            },
            5,
            "workspace/executeCommand",
        ),
    );
    let (notifications, apply_resp) = drain_notifications_until_id(&mut stdout, 5);
    let apply_edit_req = notifications
        .iter()
        .find(|msg| msg.get("method").and_then(|v| v.as_str()) == Some("workspace/applyEdit"))
        .cloned()
        .expect("server emitted workspace/applyEdit request");
    assert_eq!(
        apply_edit_req
            .pointer("/params/label")
            .and_then(|v| v.as_str()),
        Some("Safe delete")
    );
    let apply_edit_id = apply_edit_req.get("id").cloned().expect("applyEdit id");
    write_jsonrpc_message(
        &mut stdin,
        &crate::support::jsonrpc_response_ok(
            apply_edit_id,
            ApplyWorkspaceEditResponse {
                applied: true,
                failure_reason: None,
                failed_change: None,
            },
        ),
    );
    let edit: WorkspaceEdit =
        serde_json::from_value(apply_resp.get("result").cloned().expect("result"))
            .expect("decode workspace edit");
    let changes = edit.changes.expect("changes map");
    let edits = changes.get(&uri).expect("edits for uri");

    let actual = apply_lsp_edits(fixture, edits);
    assert!(!actual.contains("void used"), "method should be removed");
    assert!(!actual.contains("used()"), "usage should be removed");

    // 7) shutdown + exit
    write_jsonrpc_message(&mut stdin, &shutdown_request(6));
    let _shutdown_resp = read_response_with_id(&mut stdout, 6);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_safe_delete_targets_most_nested_method_under_cursor() {
    let _lock = crate::support::stdio_server_lock();

    // The cursor is placed on `inner`, which is declared inside a local class inside `outer`.
    // The `outer` method's declaration range *also* covers this offset; we must ensure the
    // server targets `inner` (the most-nested method) rather than `outer`.
    let fixture = r#"
class A {
    void outer() {
        class Local {
            void inner() {}

            void callInner() {
                inner();
            }
        }

        new Local().callInner();
    }
}
"#;

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    // 1) initialize
    write_jsonrpc_message(&mut stdin, &initialize_request_empty(1));
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &initialized_notification());

    // 2) open document (required for safe delete symbol IDs to be stable in the stdio server)
    let uri: Uri = "file:///test/A.java".parse().unwrap();
    write_jsonrpc_message(
        &mut stdin,
        &did_open_notification(uri.clone(), "java", 1, fixture),
    );

    // 3) request code actions at the `inner` method declaration (inside a local class)
    let decl_offset = fixture
        .find("void inner")
        .expect("method decl")
        .saturating_add("void ".len());
    let line_index = nova_core::LineIndex::new(fixture);
    let decl_pos = line_index.position(
        fixture,
        nova_core::TextSize::from(u32::try_from(decl_offset).expect("u32 offset")),
    );

    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            CodeActionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                range: Range::new(
                    Position::new(decl_pos.line, decl_pos.character),
                    Position::new(decl_pos.line, decl_pos.character),
                ),
                context: CodeActionContext::default(),
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            },
            2,
            "textDocument/codeAction",
        ),
    );

    let code_action_resp = read_response_with_id(&mut stdout, 2);
    let actions = code_action_resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("code actions array");
    let safe_delete_action = actions
        .iter()
        .find(|action| {
            action
                .get("title")
                .and_then(|v| v.as_str())
                .is_some_and(|t| t.starts_with("Safe delete"))
        })
        .expect("safe delete action");
    assert_eq!(
        safe_delete_action
            .pointer("/data/report/target/name")
            .and_then(|v| v.as_str()),
        Some("inner")
    );

    // 4) shutdown + exit
    write_jsonrpc_message(&mut stdin, &shutdown_request(3));
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
