use httpmock::prelude::*;
use lsp_types::Range;
use serde_json::json;
use std::io::BufReader;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use nova_core::{path_to_file_uri, AbsPathBuf};
use nova_lsp::text_pos::TextPos;
use tempfile::TempDir;

use crate::support::{read_response_with_id, stdio_server_lock, write_jsonrpc_message};

fn uri_for_path(path: &Path) -> String {
    let abs = AbsPathBuf::try_from(path.to_path_buf()).expect("abs path");
    path_to_file_uri(&abs).expect("file uri")
}

fn wait_for_semantic_search_indexing_after_run(
    stdin: &mut impl std::io::Write,
    stdout: &mut impl std::io::BufRead,
    prev_run_id: u64,
) -> u64 {
    for attempt in 0..200u64 {
        let id = 1000 + attempt as i64;
        write_jsonrpc_message(
            stdin,
            &json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": nova_lsp::SEMANTIC_SEARCH_INDEX_STATUS_METHOD,
                "params": {}
            }),
        );
        let resp = read_response_with_id(stdout, id);
        let Some(result) = resp.get("result") else {
            continue;
        };
        let current = result
            .get("currentRunId")
            .and_then(|v| v.as_u64())
            .unwrap_or_default();
        let done = result.get("done").and_then(|v| v.as_bool()).unwrap_or(false);
        if current > prev_run_id && done {
            return current;
        }

        std::thread::sleep(Duration::from_millis(20));
    }

    panic!("timed out waiting for semantic search workspace indexing");
}

fn request_body_contains(req: &HttpMockRequest, needle: &str) -> bool {
    let Some(body) = req.body.as_deref() else {
        return false;
    };
    let body = String::from_utf8_lossy(body);
    body.contains(needle)
}

fn request_body_not_contains(req: &HttpMockRequest, needle: &str) -> bool {
    let Some(body) = req.body.as_deref() else {
        return true;
    };
    let body = String::from_utf8_lossy(body);
    !body.contains(needle)
}

#[test]
fn stdio_server_semantic_search_reloads_config_without_restart() {
    let _lock = stdio_server_lock();

    let mock_server = MockServer::start();
    let completion_without_helper = mock_server.mock(|when, then| {
        when.method(POST)
            .path("/complete")
            .matches(|req| request_body_not_contains(req, "class Helper"));
        then.status(200).json_body(json!({ "completion": "ok-disabled" }));
    });
    let completion_with_helper = mock_server.mock(|when, then| {
        when.method(POST)
            .path("/complete")
            .matches(|req| request_body_contains(req, "class Helper"));
        then.status(200).json_body(json!({ "completion": "ok-enabled" }));
    });

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    let root_uri = uri_for_path(root);

    let src_dir = root.join("src");
    std::fs::create_dir_all(&src_dir).expect("create src dir");

    let main_path = src_dir.join("Main.java");
    let helper_path = src_dir.join("Helper.java");
    let main_text = r#"class Main { void run() { Helper.hello(); } }"#;
    let helper_text = r#"class Helper { static void hello() { } }"#;
    std::fs::write(&main_path, main_text).expect("write Main.java");
    std::fs::write(&helper_path, helper_text).expect("write Helper.java");

    let main_uri = uri_for_path(&main_path);

    // Configure AI via file, but keep semantic search disabled initially.
    let config_path = root.join("nova.config.toml");
    let config_disabled = format!(
        r#"
[ai]
enabled = true

[ai.features]
semantic_search = false

[ai.provider]
kind = "http"
url = "{endpoint}"
model = "default"
timeout_ms = 2000
max_tokens = 64
"#,
        endpoint = format!("{}/complete", mock_server.base_url())
    );
    std::fs::write(&config_path, config_disabled).expect("write config");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        // Ensure a developer's environment doesn't disable AI for this test.
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        // Avoid inheriting any legacy AI env config that would override the file.
        .env_remove("NOVA_AI_PROVIDER")
        .env_remove("NOVA_AI_ENDPOINT")
        .env_remove("NOVA_AI_MODEL")
        .env_remove("NOVA_AI_API_KEY")
        .env_remove("NOVA_AI_AUDIT_LOGGING")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    // 1) Initialize with a workspace root so workspace indexing can walk it after reload.
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

    // 2) Open the focal document while semantic search is disabled.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": main_uri,
                    "languageId": "java",
                    "version": 1,
                    "text": main_text
                }
            }
        }),
    );

    // 3) Request code actions and capture the Explain Error command payload.
    let offset = main_text.find("Helper.hello").expect("call occurrence");
    let pos = TextPos::new(main_text);
    let start = pos.lsp_position(offset).expect("start pos");
    let end = pos
        .lsp_position(offset + "Helper.hello".len())
        .expect("end pos");
    let range = Range { start, end };

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/codeAction",
            "params": {
                "textDocument": { "uri": main_uri },
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

    // 4) Execute the command and assert the prompt does NOT include Helper.java.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "workspace/executeCommand",
            "params": { "command": cmd, "arguments": args.clone() }
        }),
    );
    let exec_resp = read_response_with_id(&mut stdout, 3);
    assert_eq!(exec_resp.get("result"), Some(&json!("ok-disabled")));
    completion_without_helper.assert_hits(1);
    completion_with_helper.assert_hits(0);

    // Snapshot status before enabling semantic search (no workspace indexing should run).
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": nova_lsp::SEMANTIC_SEARCH_INDEX_STATUS_METHOD,
            "params": {}
        }),
    );
    let status_before = read_response_with_id(&mut stdout, 4);
    let prev_run_id = status_before
        .pointer("/result/currentRunId")
        .and_then(|v| v.as_u64())
        .unwrap_or_default();

    // 5) Enable semantic search via config reload.
    let config_enabled = format!(
        r#"
[ai]
enabled = true

[ai.features]
semantic_search = true

[ai.embeddings]
# Explicitly toggle this field to ensure `ai.embeddings.*` updates are applied on reload.
enabled = false

[ai.provider]
kind = "http"
url = "{endpoint}"
model = "default"
timeout_ms = 2000
max_tokens = 64
"#,
        endpoint = format!("{}/complete", mock_server.base_url())
    );
    std::fs::write(&config_path, config_enabled).expect("rewrite config");

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "workspace/didChangeConfiguration",
            "params": { "settings": {} }
        }),
    );

    // 6) Wait for the new workspace indexing run to complete.
    let _run_id = wait_for_semantic_search_indexing_after_run(&mut stdin, &mut stdout, prev_run_id);

    // 7) Execute the Explain Error command again; the prompt should now include Helper.java.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "workspace/executeCommand",
            "params": { "command": cmd, "arguments": args }
        }),
    );
    let exec_resp = read_response_with_id(&mut stdout, 5);
    assert_eq!(exec_resp.get("result"), Some(&json!("ok-enabled")));
    completion_with_helper.assert_hits(1);

    // 8) shutdown + exit
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 6, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 6);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_semantic_search_reload_updates_privacy_settings_without_restart() {
    let _lock = stdio_server_lock();

    let mock_server = MockServer::start();
    let marker = "NOVA_PRIVACY_RELOAD_MARKER_5aa801c0";
    let completion_without_paths = mock_server.mock(|when, then| {
        when.method(POST)
            .path("/complete")
            .body_contains(marker)
            .matches(|req| request_body_not_contains(req, "Helper.java"));
        then.status(200)
            .json_body(json!({ "completion": "ok-include-file-paths-disabled" }));
    });
    let completion_with_paths = mock_server.mock(|when, then| {
        when.method(POST)
            .path("/complete")
            .body_contains(marker)
            .body_contains("Helper.java");
        then.status(200)
            .json_body(json!({ "completion": "ok-include-file-paths-enabled" }));
    });

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    let root_uri = uri_for_path(root);

    let src_dir = root.join("src");
    std::fs::create_dir_all(&src_dir).expect("create src dir");

    let main_path = src_dir.join("Main.java");
    let helper_path = src_dir.join("Helper.java");
    let main_text = r#"class Main { void run() { Helper.hello(); } }"#;
    let helper_text = format!(
        r#"class Helper {{ static void hello() {{ /* {marker} */ }} }}"#
    );
    std::fs::write(&main_path, main_text).expect("write Main.java");
    std::fs::write(&helper_path, &helper_text).expect("write Helper.java");

    let main_uri = uri_for_path(&main_path);
    let helper_uri = uri_for_path(&helper_path);

    let config_path = root.join("nova.config.toml");
    let config_disabled = format!(
        r#"
[ai]
enabled = true

[ai.features]
semantic_search = true
explain_errors = true

[ai.embeddings]
enabled = false

[ai.provider]
kind = "http"
url = "{endpoint}"
model = "default"
timeout_ms = 2000
max_tokens = 64

[ai.privacy]
local_only = true
include_file_paths = false
"#,
        endpoint = format!("{}/complete", mock_server.base_url())
    );
    std::fs::write(&config_path, config_disabled).expect("write config");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        // Ensure a developer's environment doesn't disable AI for this test.
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        // Avoid inheriting any legacy AI env config that would override the file.
        .env_remove("NOVA_AI_PROVIDER")
        .env_remove("NOVA_AI_ENDPOINT")
        .env_remove("NOVA_AI_MODEL")
        .env_remove("NOVA_AI_API_KEY")
        .env_remove("NOVA_AI_AUDIT_LOGGING")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    // Initialize with a workspace root so semantic search uses `src/` as a project tree.
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

    // Open both files so the related Helper.java snippet is available without waiting for a full
    // workspace indexing pass.
    for (uri, text) in [(helper_uri.as_str(), helper_text.as_str()), (main_uri.as_str(), main_text)]
    {
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
    }

    let offset = main_text.find("Helper.hello").expect("call occurrence");
    let pos = TextPos::new(main_text);
    let start = pos.lsp_position(offset).expect("start pos");
    let end = pos
        .lsp_position(offset + "Helper.hello".len())
        .expect("end pos");
    let range = Range { start, end };

    // 1) Explain error with include_file_paths disabled; helper path should be omitted.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": nova_lsp::AI_EXPLAIN_ERROR_METHOD,
            "params": {
                "diagnosticMessage": "cannot find symbol",
                "uri": main_uri.clone(),
                "range": range.clone(),
            }
        }),
    );
    let exec_resp = read_response_with_id(&mut stdout, 2);
    assert_eq!(
        exec_resp.get("result"),
        Some(&json!("ok-include-file-paths-disabled"))
    );
    completion_without_paths.assert_hits(1);
    completion_with_paths.assert_hits(0);

    // 2) Enable include_file_paths via config reload (no restart).
    let config_enabled = format!(
        r#"
[ai]
enabled = true

[ai.features]
semantic_search = true
explain_errors = true

[ai.embeddings]
enabled = false

[ai.provider]
kind = "http"
url = "{endpoint}"
model = "default"
timeout_ms = 2000
max_tokens = 64

[ai.privacy]
local_only = true
include_file_paths = true
"#,
        endpoint = format!("{}/complete", mock_server.base_url())
    );
    std::fs::write(&config_path, config_enabled).expect("rewrite config");
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "workspace/didChangeConfiguration",
            "params": { "settings": {} }
        }),
    );

    // 3) Explain error again; Helper.java should now appear in the prompt.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": nova_lsp::AI_EXPLAIN_ERROR_METHOD,
            "params": {
                "diagnosticMessage": "cannot find symbol",
                "uri": main_uri,
                "range": range,
            }
        }),
    );
    let exec_resp = read_response_with_id(&mut stdout, 3);
    assert_eq!(
        exec_resp.get("result"),
        Some(&json!("ok-include-file-paths-enabled"))
    );
    completion_with_paths.assert_hits(1);
    completion_without_paths.assert_hits(1);

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
