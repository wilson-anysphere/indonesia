use httpmock::prelude::*;
use lsp_types::{Position, Range};
use pretty_assertions::assert_eq;
use serde_json::json;
use std::io::BufReader;
use std::path::Path;
use std::process::{Command, Stdio};

use nova_core::{path_to_file_uri, AbsPathBuf};
use nova_lsp::text_pos::TextPos;
use tempfile::TempDir;

use crate::support::{read_jsonrpc_message, read_response_with_id, write_jsonrpc_message};

fn uri_for_path(path: &Path) -> String {
    let abs = AbsPathBuf::try_from(path.to_path_buf()).expect("abs path");
    path_to_file_uri(&abs).expect("file uri")
}

#[test]
fn stdio_server_hides_ai_code_edit_actions_when_privacy_policy_disallows_edits() {
    let _lock = crate::support::stdio_server_lock();
    let ai_server = crate::support::TestAiServer::start(json!({ "completion": "mock" }));

    let temp = TempDir::new().expect("tempdir");

    let config_path = temp.path().join("nova.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[ai]
enabled = true

[ai.provider]
kind = "http"
url = "{endpoint}"
model = "default"

[ai.privacy]
local_only = false
"#,
            endpoint = format!("{}/complete", ai_server.base_url())
        ),
    )
    .expect("write config");

    let file_path = temp.path().join("Main.java");
    let file_uri = uri_for_path(&file_path);
    let text = "class Test { void foo() { } }";
    std::fs::write(&file_path, text).expect("write Main.java");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    // initialize
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

    // open a document
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": file_uri,
                    "languageId": "java",
                    "version": 1,
                    "text": text
                }
            }
        }),
    );

    // request code actions on an empty method body selection (would normally offer AI code edits).
    let selection = "void foo() { }";
    let start_offset = text.find(selection).expect("selection start");
    let end_offset = start_offset + selection.len();
    let pos = TextPos::new(text);
    let range = Range {
        start: pos.lsp_position(start_offset).expect("start"),
        end: pos.lsp_position(end_offset).expect("end"),
    };

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/codeAction",
            "params": {
                "textDocument": { "uri": file_uri },
                "range": range,
                "context": {
                    "diagnostics": [{
                        "range": range,
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

    assert!(
        actions
            .iter()
            .any(|a| a.get("title").and_then(|t| t.as_str()) == Some("Explain this error")),
        "expected explain-error action to remain available"
    );

    assert!(
        !actions.iter().any(
            |a| a.get("title").and_then(|t| t.as_str()) == Some("Generate method body with AI")
        ),
        "generate-method-body action should be hidden when code edits are disallowed"
    );
    assert!(
        !actions
            .iter()
            .any(|a| a.get("title").and_then(|t| t.as_str()) == Some("Generate tests with AI")),
        "generate-tests action should be hidden when code edits are disallowed"
    );

    // shutdown + exit
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
fn stdio_server_hides_ai_code_edit_actions_for_excluded_paths() {
    let _lock = crate::support::stdio_server_lock();
    let ai_server = crate::support::TestAiServer::start(json!({ "completion": "mock" }));

    let temp = TempDir::new().expect("tempdir");

    let config_path = temp.path().join("nova.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[ai]
enabled = true

[ai.provider]
kind = "http"
url = "{endpoint}"
model = "default"

[ai.privacy]
local_only = true
excluded_paths = ["secret/**"]
"#,
            endpoint = format!("{}/complete", ai_server.base_url())
        ),
    )
    .expect("write config");

    let secret_dir = temp.path().join("secret");
    std::fs::create_dir_all(&secret_dir).expect("create secret dir");
    let file_path = secret_dir.join("Main.java");
    let file_uri = uri_for_path(&file_path);
    let text = "class Test { void foo() { } }";
    std::fs::write(&file_path, text).expect("write secret/Main.java");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    // initialize
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

    // open a document
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": file_uri,
                    "languageId": "java",
                    "version": 1,
                    "text": text
                }
            }
        }),
    );

    // request code actions on an empty method body selection (would normally offer AI code edits).
    let selection = "void foo() { }";
    let start_offset = text.find(selection).expect("selection start");
    let end_offset = start_offset + selection.len();
    let pos = TextPos::new(text);
    let range = Range {
        start: pos.lsp_position(start_offset).expect("start"),
        end: pos.lsp_position(end_offset).expect("end"),
    };

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/codeAction",
            "params": {
                "textDocument": { "uri": file_uri },
                "range": range,
                "context": {
                    "diagnostics": [{
                        "range": range,
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

    assert!(
        actions
            .iter()
            .any(|a| a.get("title").and_then(|t| t.as_str()) == Some("Explain this error")),
        "expected explain-error action to remain available"
    );

    assert!(
        !actions.iter().any(
            |a| a.get("title").and_then(|t| t.as_str()) == Some("Generate method body with AI")
        ),
        "generate-method-body action should be hidden for excluded paths"
    );
    assert!(
        !actions
            .iter()
            .any(|a| a.get("title").and_then(|t| t.as_str()) == Some("Generate tests with AI")),
        "generate-tests action should be hidden for excluded paths"
    );

    // shutdown + exit
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
fn stdio_server_handles_ai_explain_error_code_action() {
    let _lock = crate::support::stdio_server_lock();
    let ai_server =
        crate::support::TestAiServer::start(json!({ "completion": "mock explanation" }));

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        // Ensure a developer's environment doesn't disable AI for this test.
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .env("NOVA_AI_PROVIDER", "http")
        .env(
            "NOVA_AI_ENDPOINT",
            format!("{}/complete", ai_server.base_url()),
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
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

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

    ai_server.assert_hits(1);

    // 5) shutdown + exit
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 4, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 4);

    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_ai_prompt_includes_project_and_semantic_context_when_root_is_available() {
    let _lock = crate::support::stdio_server_lock();
    let mock_server = MockServer::start();
    let mock = mock_server.mock(|when, then| {
        when.method(POST)
            .path("/complete")
            .body_contains("## Project context")
            .body_contains("## Symbol/type info");
        then.status(200)
            .json_body(json!({ "completion": "mock explanation" }));
    });

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    let root_uri = uri_for_path(root);

    let src_dir = root.join("src");
    std::fs::create_dir_all(&src_dir).expect("create src dir");

    let file_path = src_dir.join("Main.java");
    let file_uri = uri_for_path(&file_path);
    let text = r#"class Main { void run() { String s = "hi"; } }"#;
    std::fs::write(&file_path, text).expect("write Main.java");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        // Ensure a developer's environment doesn't disable AI for this test.
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
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

    // 1) initialize with a workspace root so project context can be loaded.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "rootUri": root_uri, "capabilities": {} }
        }),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    // 2) open an on-disk document so hover/type info has a stable path.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": file_uri,
                    "languageId": "java",
                    "version": 1,
                    "text": text
                }
            }
        }),
    );

    // 3) request code actions with a diagnostic range over an identifier so hover returns info.
    let offset = text.find("s =").expect("variable occurrence");
    let pos = TextPos::new(text);
    let start = pos.lsp_position(offset).expect("start pos");
    let end = pos.lsp_position(offset + 1).expect("end pos");
    let range = Range { start, end };

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/codeAction",
            "params": {
                "textDocument": { "uri": file_uri },
                "range": range,
                "context": {
                    "diagnostics": [{
                        "range": range,
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

    // 4) Execute the command (this triggers the mock LLM call, which asserts on prompt contents).
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "workspace/executeCommand",
            "params": {
                "command": cmd,
                "arguments": args
            }
        }),
    );
    let exec_resp = read_response_with_id(&mut stdout, 3);
    assert_eq!(exec_resp.get("result"), Some(&json!("mock explanation")));
    mock.assert_hits(1);

    // 5) shutdown + exit
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 4, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 4);

    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_chunks_long_ai_explain_error_log_messages() {
    let _lock = crate::support::stdio_server_lock();
    let mock_server = MockServer::start();

    // Large enough that `nova-lsp` must split it across multiple `window/logMessage`
    // notifications (see `AI_LOG_MESSAGE_CHUNK_BYTES`).
    let long = "Nova AI output ".repeat(4_000);
    let mock = mock_server.mock(|when, then| {
        when.method(POST).path("/complete");
        then.status(200).json_body(json!({ "completion": long }));
    });

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        // Ensure a developer's environment doesn't disable AI for this test.
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
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

    // initialize
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

    // open document so the server can attach snippets.
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

    // request code actions with a diagnostic.
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

    // execute command (triggers the mock AI call).
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

    let mut progress_kinds = Vec::new();
    let mut output_chunks = Vec::new();
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

        if msg.get("method").and_then(|v| v.as_str()) == Some("window/logMessage") {
            if let Some(text) = msg
                .get("params")
                .and_then(|p| p.get("message"))
                .and_then(|m| m.as_str())
            {
                if text.starts_with("AI explainError") {
                    let (_, chunk) = text
                        .split_once(": ")
                        .expect("AI chunk messages should contain ': ' delimiter");
                    output_chunks.push(chunk.to_string());
                }
            }
            continue;
        }

        if msg.get("id").and_then(|v| v.as_i64()) == Some(3) {
            break msg;
        }
        // Other notification/response; ignore.
    };

    let result = exec_resp
        .get("result")
        .and_then(|v| v.as_str())
        .expect("executeCommand result string");
    assert_eq!(result, long.as_str());
    assert!(progress_kinds.contains(&"begin".to_string()));
    assert!(progress_kinds.contains(&"end".to_string()));
    assert!(
        output_chunks.len() > 1,
        "expected long AI output to be chunked, got {output_chunks:?}"
    );
    assert_eq!(output_chunks.join(""), long);

    mock.assert_hits(1);

    // shutdown + exit
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 4, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 4);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_completion_ranking_misconfiguration_falls_back_to_baseline_completions() {
    let _lock = crate::support::stdio_server_lock();
    let temp = TempDir::new().expect("tempdir");
    let config_path = temp.path().join("nova.toml");
    std::fs::write(
        &config_path,
        r#"
[ai]
enabled = true

[ai.privacy]
local_only = false

[ai.provider]
kind = "open_ai"

[ai.features]
completion_ranking = true
"#,
    )
    .expect("write config");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .env_remove("NOVA_AI_PROVIDER")
        .env_remove("NOVA_AI_ENDPOINT")
        .env_remove("NOVA_AI_MODEL")
        .env_remove("NOVA_AI_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
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

    let uri = "file:///Test.java";
    let text = "class Test { void run() { String s = \"hi\"; s. } }";
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": uri,
                    "languageId": "java",
                    "version": 1,
                    "text": text
                }
            }
        }),
    );

    let offset = text.find("s.").expect("cursor");
    let pos = TextPos::new(text)
        .lsp_position(offset + 2)
        .expect("position");
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/completion",
            "params": {
                "textDocument": { "uri": uri },
                "position": pos
            }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 2);

    assert!(
        resp.get("error").is_none(),
        "expected completion response to succeed, got: {resp:#?}"
    );

    let items = resp
        .get("result")
        .and_then(|v| v.get("items"))
        .and_then(|v| v.as_array())
        .expect("completion list");
    assert!(!items.is_empty(), "expected completion items");

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
fn stdio_server_extracts_utf16_ranges_for_ai_code_actions() {
    let _lock = crate::support::stdio_server_lock();
    // The code action request itself should not invoke the provider, but we need
    // a valid endpoint so the server considers AI configured.
    let ai_server =
        crate::support::TestAiServer::start(json!({ "completion": "unused in this test" }));

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        // Ensure a developer's environment doesn't disable AI for this test.
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .env("NOVA_AI_PROVIDER", "http")
        // Ensure patch-based AI code actions are allowed so this test exercises UTF-16 range
        // extraction rather than privacy gating.
        .env("NOVA_AI_LOCAL_ONLY", "1")
        .env(
            "NOVA_AI_ENDPOINT",
            format!("{}/complete", ai_server.base_url()),
        )
        .env("NOVA_AI_MODEL", "default")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    // initialize
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

    let uri = "file:///Test.java";
    let text = "class Test { void run() { String s = \"😀\"; } }\n";
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": uri,
                    "languageId": "java",
                    "version": 1,
                    "text": text
                }
            }
        }),
    );

    let emoji_offset = text.find('😀').expect("emoji present");
    let pos = TextPos::new(text);
    let start = pos.lsp_position(emoji_offset).expect("start pos");
    let end = pos
        .lsp_position(emoji_offset + '😀'.len_utf8())
        .expect("end pos");

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/codeAction",
            "params": {
                "textDocument": { "uri": uri },
                "range": Range::new(start, end),
                "context": { "diagnostics": [] }
            }
        }),
    );

    let resp = read_response_with_id(&mut stdout, 2);
    let actions = resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("code actions array");

    let gen_tests = actions
        .iter()
        .find(|a| a.get("title").and_then(|t| t.as_str()) == Some("Generate tests with AI"))
        .expect("generate tests action");
    let cmd = gen_tests
        .get("command")
        .and_then(|c| c.get("command"))
        .and_then(|v| v.as_str())
        .expect("command string");
    assert_eq!(cmd, nova_ide::COMMAND_GENERATE_TESTS);

    let args = gen_tests
        .get("command")
        .and_then(|c| c.get("arguments"))
        .and_then(|v| v.as_array())
        .expect("arguments");
    let target = args[0]
        .get("target")
        .and_then(|v| v.as_str())
        .expect("target");
    assert_eq!(target, "😀");
    assert_eq!(
        ai_server.hits(),
        0,
        "codeAction should not call the AI provider"
    );

    // shutdown + exit
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
fn stdio_server_rejects_surrogate_pair_interior_ranges_for_ai_code_actions() {
    let _lock = crate::support::stdio_server_lock();
    let ai_server =
        crate::support::TestAiServer::start(json!({ "completion": "unused in this test" }));

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        // Ensure a developer's environment doesn't disable AI for this test.
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .env("NOVA_AI_PROVIDER", "http")
        // Ensure patch-based AI code actions are allowed so this test exercises UTF-16 range
        // validation rather than privacy gating.
        .env("NOVA_AI_LOCAL_ONLY", "1")
        .env(
            "NOVA_AI_ENDPOINT",
            format!("{}/complete", ai_server.base_url()),
        )
        .env("NOVA_AI_MODEL", "default")
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

    let uri = "file:///Test.java";
    let text = "class Test { String s = \"😀\"; }\n";
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": uri,
                    "languageId": "java",
                    "version": 1,
                    "text": text
                }
            }
        }),
    );

    let emoji_offset = text.find('😀').expect("emoji present");
    let pos = TextPos::new(text);
    let start = pos.lsp_position(emoji_offset).expect("start pos");
    let inside = Position::new(start.line, start.character + 1);
    let end = Position::new(start.line, start.character + 2);

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/codeAction",
            "params": {
                "textDocument": { "uri": uri },
                "range": Range::new(inside, end),
                "context": { "diagnostics": [] }
            }
        }),
    );

    let resp = read_response_with_id(&mut stdout, 2);
    let actions = resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("code actions array");

    assert!(
        actions
            .iter()
            .all(|a| a.get("title").and_then(|t| t.as_str()) != Some("Generate tests with AI")),
        "expected no generate tests action for invalid UTF-16 range, got: {actions:#?}"
    );
    assert_eq!(
        ai_server.hits(),
        0,
        "codeAction should not call the AI provider"
    );

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
fn stdio_server_rejects_cloud_ai_code_edits_when_anonymization_is_enabled() {
    let _lock = crate::support::stdio_server_lock();
    let ai_server = crate::support::TestAiServer::start(json!({ "completion": "unused" }));

    let temp = TempDir::new().expect("tempdir");
    let config_path = temp.path().join("nova.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[ai]
enabled = true

[ai.provider]
kind = "http"
url = "{}/complete"
model = "default"

[ai.privacy]
local_only = false
"#,
            ai_server.base_url()
        ),
    )
    .expect("write config");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        .env_remove("NOVA_AI_PROVIDER")
        .env_remove("NOVA_AI_ENDPOINT")
        .env_remove("NOVA_AI_MODEL")
        .env_remove("NOVA_AI_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    // initialize
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

    // Open a document. We won't expect patch-based AI code actions to be advertised in this mode,
    // but we still validate that attempting to execute the command is rejected by the privacy
    // policy (defense in depth).
    let uri = "file:///Test.java";
    let text = "class Test {\n    void run() {\n    }\n}\n";
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": uri,
                    "languageId": "java",
                    "version": 1,
                    "text": text
                }
            }
        }),
    );

    // Request code actions: the patch-based AI code-edit actions should be hidden in cloud mode
    // with anonymization enabled (default).
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/codeAction",
            "params": {
                "textDocument": { "uri": uri },
                "range": { "start": { "line": 1, "character": 0 }, "end": { "line": 1, "character": 0 } },
                "context": { "diagnostics": [] }
            }
        }),
    );
    let code_actions_resp = read_response_with_id(&mut stdout, 2);
    let actions = code_actions_resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("code actions array");
    assert!(
        actions.iter().all(
            |a| a.get("title").and_then(|t| t.as_str()) != Some("Generate method body with AI")
        ),
        "expected no code-edit actions in cloud anonymized mode, got: {actions:#?}"
    );
    assert!(
        actions
            .iter()
            .all(|a| a.get("title").and_then(|t| t.as_str()) != Some("Generate tests with AI")),
        "expected no code-edit actions in cloud anonymized mode, got: {actions:#?}"
    );

    // Execute command: in cloud mode, anonymization is enabled by default and code edits should be
    // rejected before any model call is made.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "workspace/executeCommand",
            "params": {
                "command": nova_ide::COMMAND_GENERATE_METHOD_BODY,
                "arguments": [{
                    "method_signature": "void run()",
                    "context": null,
                    "uri": uri
                }]
            }
        }),
    );

    let exec_resp = read_response_with_id(&mut stdout, 3);
    let err_msg = exec_resp
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .expect("expected executeCommand to return an error");
    assert!(
        err_msg.contains(
            "AI code edits are disabled when identifier anonymization is enabled in cloud mode"
        ),
        "expected CodeEditPolicyError in error message, got: {err_msg}"
    );
    assert_eq!(
        ai_server.hits(),
        0,
        "expected no AI provider calls when code edits are blocked by policy"
    );

    // shutdown + exit
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 4, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 4);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
