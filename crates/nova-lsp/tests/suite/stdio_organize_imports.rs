use lsp_types::{TextEdit, Uri, WorkspaceEdit};
use pretty_assertions::assert_eq;
use serde_json::json;
use std::io::BufReader;
use std::process::{Command, Stdio};
use std::str::FromStr;

use crate::support::{read_jsonrpc_message, read_response_with_id, write_jsonrpc_message};

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

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "capabilities": {} }
        }),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

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

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": { "textDocument": { "uri": uri, "languageId": "java", "version": 1, "text": source } }
        }),
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/java/organizeImports",
            "params": { "uri": uri }
        }),
    );

    let mut apply_edit = None;
    let response = loop {
        let msg = read_jsonrpc_message(&mut stdout);

        if msg.get("method").and_then(|v| v.as_str()) == Some("workspace/applyEdit") {
            apply_edit = Some(msg.clone());
            let id = msg.get("id").cloned().expect("applyEdit id");
            write_jsonrpc_message(
                &mut stdin,
                &json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "applied": true }
                }),
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

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 3, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
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

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "capabilities": {} }
        }),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

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

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": { "textDocument": { "uri": uri, "languageId": "java", "version": 1, "text": source } }
        }),
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/codeAction",
            "params": {
                "textDocument": { "uri": uri },
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 0, "character": 0 }
                },
                "context": { "diagnostics": [] }
            }
        }),
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

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 3, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
