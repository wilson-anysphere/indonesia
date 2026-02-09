use serde_json::json;
use std::fs;
use std::io::BufReader;
use std::process::{Command, Stdio};

use lsp_types::Range;
use nova_lsp::text_pos::TextPos;
use tempfile::TempDir;

use crate::support;

fn spawn_lsp_with_config(config_path: &std::path::Path) -> std::process::Child {
    Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(config_path)
        // The test config file should be authoritative; clear any legacy env-var AI wiring that
        // could override `--config` (common in developer shells).
        .env_remove("NOVA_AI_PROVIDER")
        .env_remove("NOVA_AI_ENDPOINT")
        .env_remove("NOVA_AI_MODEL")
        .env_remove("NOVA_AI_API_KEY")
        .env_remove("NOVA_AI_AUDIT_LOGGING")
        // Ensure a developer's environment doesn't disable AI for this test.
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .env_remove("NOVA_DISABLE_AI_CODE_ACTIONS")
        .env_remove("NOVA_DISABLE_AI_CODE_REVIEW")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp")
}

#[test]
fn stdio_server_respects_ai_feature_toggle_code_actions() {
    let _lock = support::stdio_server_lock();

    let ai_server = support::TestAiServer::start(json!({ "completion": "mock" }));
    let endpoint = format!("{}/complete", ai_server.base_url());

    let temp = TempDir::new().expect("tempdir");
    let config_path = temp.path().join("nova.toml");
    fs::write(
        &config_path,
        format!(
            r#"
[ai]
enabled = true

[ai.features]
code_actions = false

[ai.provider]
kind = "http"
url = "{endpoint}"
model = "default"
"#
        ),
    )
    .expect("write config");

    let file_path = temp.path().join("Main.java");
    let file_uri = support::file_uri_string(&file_path);
    let text = "class Test { void foo() { } }";
    fs::write(&file_path, text).expect("write Main.java");

    let mut child = spawn_lsp_with_config(&config_path);
    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "capabilities": {} }
        }),
    );
    let _initialize_resp = support::read_response_with_id(&mut stdout, 1);
    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    // open a document
    support::write_jsonrpc_message(
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

    support::write_jsonrpc_message(
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

    let code_actions_resp = support::read_response_with_id(&mut stdout, 2);
    let actions = code_actions_resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("code actions array");

    assert!(
        !actions.iter().any(|a| {
            a.get("title").and_then(|t| t.as_str()) == Some("Generate method body with AI")
        }),
        "generate-method-body action should be hidden when ai.features.code_actions=false"
    );
    assert!(
        !actions.iter().any(|a| a.get("title").and_then(|t| t.as_str()) == Some("Generate tests with AI")),
        "generate-tests action should be hidden when ai.features.code_actions=false"
    );

    // Calling the custom request should fail with an actionable error.
    let method_body_params = json!({
        "method_signature": "void foo()",
        "uri": file_uri,
        "range": { "start": { "line": range.start.line, "character": range.start.character }, "end": { "line": range.end.line, "character": range.end.character } }
    });
    support::write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "nova/ai/generateMethodBody",
            "params": method_body_params,
        }),
    );
    let generate_resp = support::read_response_with_id(&mut stdout, 3);
    let err = generate_resp
        .get("error")
        .and_then(|v| v.as_object())
        .expect("expected error response");
    assert_eq!(err.get("code").and_then(|v| v.as_i64()), Some(-32600));
    let message = err
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(
        message.contains("ai.features.code_actions"),
        "expected error message to mention ai.features.code_actions, got: {message:?}"
    );
    assert_eq!(
        err.get("data")
            .and_then(|v| v.get("kind"))
            .and_then(|v| v.as_str()),
        Some("disabled"),
        "expected structured disabled error kind"
    );
    assert_eq!(
        err.get("data")
            .and_then(|v| v.get("feature"))
            .and_then(|v| v.as_str()),
        Some("ai.features.code_actions"),
        "expected structured disabled error feature"
    );

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 4, "method": "shutdown" }),
    );
    let _shutdown_resp = support::read_response_with_id(&mut stdout, 4);
    support::write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
    ai_server.assert_hits(0);
}

#[test]
fn stdio_server_respects_ai_feature_toggle_explain_errors() {
    let _lock = support::stdio_server_lock();

    let ai_server = support::TestAiServer::start(json!({ "completion": "mock" }));
    let endpoint = format!("{}/complete", ai_server.base_url());

    let temp = TempDir::new().expect("tempdir");
    let config_path = temp.path().join("nova.toml");
    fs::write(
        &config_path,
        format!(
            r#"
[ai]
enabled = true

[ai.features]
explain_errors = false

[ai.provider]
kind = "http"
url = "{endpoint}"
model = "default"
"#
        ),
    )
    .expect("write config");

    let file_path = temp.path().join("Main.java");
    let file_uri = support::file_uri_string(&file_path);
    let text = "class Test { void foo() { } }";
    fs::write(&file_path, text).expect("write Main.java");

    let mut child = spawn_lsp_with_config(&config_path);
    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "capabilities": {} }
        }),
    );
    let _initialize_resp = support::read_response_with_id(&mut stdout, 1);
    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    support::write_jsonrpc_message(
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

    let selection = "void foo() { }";
    let start_offset = text.find(selection).expect("selection start");
    let end_offset = start_offset + selection.len();
    let pos = TextPos::new(text);
    let range = Range {
        start: pos.lsp_position(start_offset).expect("start"),
        end: pos.lsp_position(end_offset).expect("end"),
    };

    support::write_jsonrpc_message(
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

    let code_actions_resp = support::read_response_with_id(&mut stdout, 2);
    let actions = code_actions_resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("code actions array");

    assert!(
        !actions.iter().any(|a| a.get("title").and_then(|t| t.as_str()) == Some("Explain this error")),
        "explain-error action should be hidden when ai.features.explain_errors=false"
    );

    // The patch-based actions should still be present (code_actions defaults to true).
    assert!(
        actions.iter().any(|a| {
            a.get("title").and_then(|t| t.as_str()) == Some("Generate method body with AI")
        }),
        "expected generate-method-body action to remain available"
    );

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "nova/ai/explainError",
            "params": {
                "diagnostic_message": "cannot find symbol",
                "uri": file_uri,
                "range": { "start": { "line": range.start.line, "character": range.start.character }, "end": { "line": range.end.line, "character": range.end.character } }
            }
        }),
    );
    let explain_resp = support::read_response_with_id(&mut stdout, 3);
    let err = explain_resp
        .get("error")
        .and_then(|v| v.as_object())
        .expect("expected error response");
    assert_eq!(err.get("code").and_then(|v| v.as_i64()), Some(-32600));
    let message = err
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(
        message.contains("ai.features.explain_errors"),
        "expected error message to mention ai.features.explain_errors, got: {message:?}"
    );
    assert_eq!(
        err.get("data")
            .and_then(|v| v.get("kind"))
            .and_then(|v| v.as_str()),
        Some("disabled"),
        "expected structured disabled error kind"
    );
    assert_eq!(
        err.get("data")
            .and_then(|v| v.get("feature"))
            .and_then(|v| v.as_str()),
        Some("ai.features.explain_errors"),
        "expected structured disabled error feature"
    );

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 4, "method": "shutdown" }),
    );
    let _shutdown_resp = support::read_response_with_id(&mut stdout, 4);
    support::write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
    ai_server.assert_hits(0);
}
