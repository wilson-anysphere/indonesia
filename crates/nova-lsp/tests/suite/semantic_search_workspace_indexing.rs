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

use crate::support::{read_response_with_id, write_jsonrpc_message};

fn uri_for_path(path: &Path) -> String {
    let abs = AbsPathBuf::try_from(path.to_path_buf()).expect("abs path");
    path_to_file_uri(&abs).expect("file uri")
}

fn wait_for_semantic_search_indexing(
    stdin: &mut impl std::io::Write,
    stdout: &mut impl std::io::BufRead,
) {
    // The workspace is tiny (two files), so indexing should finish quickly. Still, poll with a
    // bounded timeout to keep this deterministic across platforms/CI.
    for attempt in 0..100u64 {
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
        if resp
            .get("result")
            .and_then(|r| r.get("done"))
            .and_then(|v| v.as_bool())
            == Some(true)
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for semantic search workspace indexing");
}

#[test]
fn stdio_server_semantic_search_indexes_non_open_workspace_files_for_ai_context() {
    let _lock = crate::support::stdio_server_lock();

    let mock_server = MockServer::start();
    let mock = mock_server.mock(|when, then| {
        when.method(POST)
            .path("/complete")
            // The related code section should include content from Helper.java even though it was
            // never opened by the client.
            .body_contains("class Helper");
        then.status(200).json_body(json!({ "completion": "ok" }));
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

    // Configure AI + semantic search purely via config so we can enable `ai.features.semantic_search`.
    let config_path = root.join("nova.config.toml");
    let config = format!(
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
    std::fs::write(&config_path, config).expect("write config");

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
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    // 1) initialize with a workspace root so workspace indexing can walk it.
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

    // 2) Wait for the background workspace semantic-search index to be populated.
    wait_for_semantic_search_indexing(&mut stdin, &mut stdout);

    // 3) Open only the focal document (Main.java). Helper.java stays closed.
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

    // 4) Request code actions with a diagnostic over an identifier that should match Helper.java.
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

    // 5) Execute the command (this triggers the mock LLM call, which asserts on prompt contents).
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
    assert_eq!(exec_resp.get("result"), Some(&json!("ok")));
    mock.assert_hits(1);

    // 6) shutdown + exit
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
