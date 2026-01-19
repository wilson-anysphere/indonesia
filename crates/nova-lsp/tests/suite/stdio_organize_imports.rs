use lsp_types::{
    ApplyWorkspaceEditResponse, CodeActionContext, CodeActionParams, PartialResultParams,
    TextDocumentIdentifier, Uri, WorkDoneProgressParams, WorkspaceEdit,
};
use nova_test_utils::apply_lsp_edits;
use pretty_assertions::assert_eq;
use serde_json::{Map, Value};
use std::io::BufReader;
use std::process::{Command, Stdio};
use std::str::FromStr;

use crate::support::{
    did_open_notification, exit_notification, initialize_request_empty, initialized_notification,
    jsonrpc_request, read_jsonrpc_message, read_response_with_id, shutdown_request,
    write_jsonrpc_message,
};

#[test]
fn stdio_server_supports_java_organize_imports_request() {
    let _lock = crate::support::stdio_server_lock();
    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    write_jsonrpc_message(&mut stdin, &initialize_request_empty(1));
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &initialized_notification());

    let uri = "file:///test/Foo.java";
    let source = r#"package com.example;

import java.util.List;
import java.io.File;
import java.util.ArrayList;
import java.util.Collections;
public class Foo {
    private List<String> xs = new ArrayList<>();
}
"#;

    write_jsonrpc_message(&mut stdin, &did_open_notification(uri, "java", 1, source));

    let mut params = Map::new();
    params.insert("uri".to_string(), Value::String(uri.to_string()));
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(Value::Object(params), 2, "nova/java/organizeImports"),
    );

    let mut apply_edit = None;
    let response = loop {
        let msg = read_jsonrpc_message(&mut stdout);

        if msg.get("method").and_then(|v| v.as_str()) == Some("workspace/applyEdit") {
            apply_edit = Some(msg.clone());
            let id = msg.get("id").cloned().expect("applyEdit id");
            write_jsonrpc_message(
                &mut stdin,
                &crate::support::jsonrpc_response_ok(
                    id,
                    ApplyWorkspaceEditResponse {
                        applied: true,
                        failure_reason: None,
                        failed_change: None,
                    },
                ),
            );
            continue;
        }

        if msg.get("id").and_then(|v| v.as_i64()) == Some(2) {
            break msg;
        }
    };

    let apply_edit = apply_edit.expect("server emitted workspace/applyEdit request");
    assert_eq!(
        apply_edit.get("method").and_then(|v| v.as_str()),
        Some("workspace/applyEdit")
    );

    let result = response.get("result").cloned().expect("result");
    assert_eq!(result.get("applied").and_then(|v| v.as_bool()), Some(true));

    let edit_value = result.get("edit").cloned().expect("edit");
    let edit: WorkspaceEdit = serde_json::from_value(edit_value).expect("workspace edit");
    let uri = Uri::from_str(uri).expect("uri");
    let changes = edit.changes.expect("changes map");
    let edits = changes.get(&uri).expect("edits for uri");

    let actual = apply_lsp_edits(source, edits);
    let expected = r#"package com.example;

import java.util.ArrayList;
import java.util.List;

public class Foo {
    private List<String> xs = new ArrayList<>();
}
"#;
    assert_eq!(actual, expected);

    write_jsonrpc_message(&mut stdin, &shutdown_request(3));
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_offers_source_organize_imports_code_action() {
    let _lock = crate::support::stdio_server_lock();
    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    write_jsonrpc_message(&mut stdin, &initialize_request_empty(1));
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &initialized_notification());

    let uri = "file:///test/Foo.java";
    let source = r#"package com.example;

import java.util.List;
import java.io.File;
import java.util.ArrayList;
import java.util.Collections;
public class Foo {
    private List<String> xs = new ArrayList<>();
}
"#;

    write_jsonrpc_message(&mut stdin, &did_open_notification(uri, "java", 1, source));

    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            CodeActionParams {
                text_document: TextDocumentIdentifier {
                    uri: Uri::from_str(uri).expect("uri"),
                },
                range: lsp_types::Range::new(
                    lsp_types::Position::new(0, 0),
                    lsp_types::Position::new(0, 0),
                ),
                context: CodeActionContext {
                    diagnostics: Vec::new(),
                    only: None,
                    trigger_kind: None,
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
            2,
            "textDocument/codeAction",
        ),
    );

    let response = read_response_with_id(&mut stdout, 2);
    let actions_value = response.get("result").cloned().expect("result");
    let actions: Vec<lsp_types::CodeActionOrCommand> =
        serde_json::from_value(actions_value).expect("decode actions");

    let action = actions
        .iter()
        .find_map(|action| match action {
            lsp_types::CodeActionOrCommand::CodeAction(action)
                if action.kind == Some(lsp_types::CodeActionKind::SOURCE_ORGANIZE_IMPORTS) =>
            {
                Some(action)
            }
            _ => None,
        })
        .expect("expected `source.organizeImports` code action");

    assert_eq!(action.title, "Organize imports");
    assert_eq!(
        action.is_preferred,
        Some(true),
        "Organize imports should be preferred"
    );

    let edit = action.edit.as_ref().expect("workspace edit");
    let uri = Uri::from_str(uri).expect("uri");
    let changes = edit.changes.as_ref().expect("changes map");
    let edits = changes.get(&uri).expect("edits for uri");

    let actual = apply_lsp_edits(source, edits);
    let expected = r#"package com.example;

import java.util.ArrayList;
import java.util.List;

public class Foo {
    private List<String> xs = new ArrayList<>();
}
"#;
    assert_eq!(actual, expected);

    write_jsonrpc_message(&mut stdin, &shutdown_request(3));
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
