use httpmock::prelude::*;
use lsp_types::Range;
use serde_json::json;
use std::io::BufReader;
use std::process::{Command, Stdio};
use std::time::Duration;

use nova_lsp::text_pos::TextPos;
use tempfile::TempDir;

use crate::support::{file_uri_string, read_response_with_id, stdio_server_lock, write_jsonrpc_message};

fn wait_for_semantic_search_indexing(
    stdin: &mut impl std::io::Write,
    stdout: &mut impl std::io::BufRead,
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
        if current != 0 && done {
            return current;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    panic!("timed out waiting for semantic search workspace indexing");
}

fn wait_for_semantic_search_indexing_after_run(
    stdin: &mut impl std::io::Write,
    stdout: &mut impl std::io::BufRead,
    prev_run_id: u64,
) -> u64 {
    for attempt in 0..200u64 {
        let id = 2000 + attempt as i64;
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

    panic!("timed out waiting for semantic search workspace indexing after config reload");
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
fn semantic_search_excluded_paths_reload_removes_related_code() {
    let _lock = stdio_server_lock();

    let mock_server = MockServer::start();
    let completion_with_helper = mock_server.mock(|when, then| {
        when.method(POST)
            .path("/complete")
            .matches(|req| request_body_contains(req, "class Helper"));
        then.status(200).json_body(json!({ "completion": "ok-included" }));
    });
    let completion_without_helper = mock_server.mock(|when, then| {
        when.method(POST)
            .path("/complete")
            .matches(|req| request_body_not_contains(req, "class Helper"));
        then.status(200).json_body(json!({ "completion": "ok-excluded" }));
    });

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    let root_uri = file_uri_string(root);

    let src_dir = root.join("src");
    std::fs::create_dir_all(&src_dir).expect("create src dir");

    let main_path = src_dir.join("Main.java");
    let helper_path = src_dir.join("Helper.java");
    let main_text = r#"class Main { void run() { Helper.hello(); } }"#;
    let helper_text = r#"class Helper { static void hello() { } }"#;
    std::fs::write(&main_path, main_text).expect("write Main.java");
    std::fs::write(&helper_path, helper_text).expect("write Helper.java");

    let main_uri = file_uri_string(&main_path);

    // Enable semantic search initially and keep privacy exclusions empty.
    let config_path = root.join("nova.config.toml");
    let config_enabled = format!(
        r#"
[ai]
enabled = true

[ai.features]
semantic_search = true

[ai.provider]
kind = "http"
url = "{endpoint}"
model = "default"
timeout_ms = 2000
max_tokens = 64
"#,
        endpoint = format!("{}/complete", mock_server.base_url())
    );
    std::fs::write(&config_path, config_enabled).expect("write config");

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

    let initial_run_id = wait_for_semantic_search_indexing(&mut stdin, &mut stdout);

    // Open only the focal document.
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

    // Request code actions with a diagnostic over an identifier that should match Helper.java.
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

    // Execute the command once; workspace indexing should have added Helper.java as related code.
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
    assert_eq!(exec_resp.get("result"), Some(&json!("ok-included")));
    completion_with_helper.assert_hits(1);

    // Now exclude Helper.java from AI via config reload. Semantic search indexing should also omit
    // it (so it cannot be surfaced as related code).
    let config_exclude_helper = format!(
        r#"
[ai]
enabled = true

[ai.features]
semantic_search = true

[ai.privacy]
excluded_paths = ["src/Helper.java"]

[ai.provider]
kind = "http"
url = "{endpoint}"
model = "default"
timeout_ms = 2000
max_tokens = 64
"#,
        endpoint = format!("{}/complete", mock_server.base_url())
    );
    std::fs::write(&config_path, config_exclude_helper).expect("rewrite config");
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "workspace/didChangeConfiguration",
            "params": { "settings": {} }
        }),
    );

    let _run_id = wait_for_semantic_search_indexing_after_run(&mut stdin, &mut stdout, initial_run_id);

    // Execute again: prompt should no longer include Helper.java.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "workspace/executeCommand",
            "params": { "command": cmd, "arguments": args }
        }),
    );
    let exec_resp = read_response_with_id(&mut stdout, 4);
    assert_eq!(exec_resp.get("result"), Some(&json!("ok-excluded")));
    completion_without_helper.assert_hits(1);

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 5, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 5);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

