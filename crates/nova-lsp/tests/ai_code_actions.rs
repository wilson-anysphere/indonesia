use httpmock::prelude::*;
mod support;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::io::BufReader;
use std::process::{Command, Stdio};

use support::{read_jsonrpc_message, read_response_with_id, write_jsonrpc_message};

#[test]
fn stdio_server_handles_ai_explain_error_code_action() {
    let mock_server = MockServer::start();
    let mock = mock_server.mock(|when, then| {
        when.method(POST).path("/complete");
        then.status(200)
            .json_body(json!({ "completion": "mock explanation" }));
    });

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .env("NOVA_AI_PROVIDER", "http")
        .env(
            "NOVA_AI_ENDPOINT",
            format!("{}/complete", mock_server.base_url()),
        )
        .env("NOVA_AI_MODEL", "default")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    // 1) initialize
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

    // 2) open a document so the server can attach code snippets.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": "file:///Test.java",
                    "languageId": "java",
                    "version": 1,
                    "text": "class Test { void run() { unknown(); } }"
                }
            }
        }),
    );

    // 3) request code actions with a diagnostic.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/codeAction",
            "params": {
                "textDocument": { "uri": "file:///Test.java" },
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 0, "character": 10 }
                },
                "context": {
                    "diagnostics": [{
                        "range": {
                            "start": { "line": 0, "character": 0 },
                            "end": { "line": 0, "character": 10 }
                        },
                        "message": "cannot find symbol"
                    }]
                }
            }
        }),
    );

    let code_actions_resp = read_response_with_id(&mut stdout, 2);
    let actions = code_actions_resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("code actions array");

    let explain = actions
        .iter()
        .find(|a| a.get("title").and_then(|t| t.as_str()) == Some("Explain this error"))
        .expect("explain code action");

    let cmd = explain
        .get("command")
        .expect("command")
        .get("command")
        .and_then(|v| v.as_str())
        .expect("command string");

    let args = explain
        .get("command")
        .expect("command")
        .get("arguments")
        .cloned()
        .expect("arguments");

    assert_eq!(cmd, nova_ide::COMMAND_EXPLAIN_ERROR);

    // 4) Execute the command (this triggers the mock LLM call).
    let progress_token = json!("progress-token");
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "workspace/executeCommand",
            "params": {
                "command": cmd,
                "arguments": args,
                "workDoneToken": progress_token.clone()
            }
        }),
    );

    // Collect work-done progress notifications emitted during the AI request.
    let mut progress_kinds = Vec::new();
    let exec_resp = loop {
        let msg = read_jsonrpc_message(&mut stdout);
        if msg.get("method").and_then(|v| v.as_str()) == Some("$/progress") {
            if msg.get("params").and_then(|p| p.get("token")) == Some(&progress_token) {
                if let Some(kind) = msg
                    .get("params")
                    .and_then(|p| p.get("value"))
                    .and_then(|v| v.get("kind"))
                    .and_then(|v| v.as_str())
                {
                    progress_kinds.push(kind.to_string());
                }
            }
            continue;
        }

        if msg.get("id").and_then(|v| v.as_i64()) == Some(3) {
            break msg;
        }
        // Notification or unrelated response; ignore.
    };
    assert_eq!(exec_resp.get("result"), Some(&json!("mock explanation")));
    assert!(progress_kinds.contains(&"begin".to_string()));
    assert!(progress_kinds.contains(&"end".to_string()));

    mock.assert_hits(1);

    // 5) shutdown + exit
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "id": 4, "method": "shutdown" }));
    let _shutdown_resp = read_response_with_id(&mut stdout, 4);

    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
