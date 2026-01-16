use httpmock::prelude::*;
use lsp_types::{
    CodeActionContext, CodeActionParams, Diagnostic, ExecuteCommandParams, FileChangeType,
    FileEvent, Range, TextDocumentIdentifier, Uri, WorkDoneProgressParams, WorkspaceFolder,
    WorkspaceFoldersChangeEvent,
};
use serde_json::Value;
use std::io::BufReader;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use nova_core::{path_to_file_uri, AbsPathBuf};
use nova_lsp::text_pos::TextPos;
use tempfile::TempDir;

use crate::support::{
    did_open_notification, empty_object, exit_notification, initialize_request_with_root_uri,
    initialized_notification, jsonrpc_notification, jsonrpc_request, read_response_with_id,
    shutdown_request, stdio_server_lock, write_jsonrpc_message,
};

fn completion_response_ok() -> Value {
    Value::Object({
        let mut value = serde_json::Map::new();
        value.insert("completion".to_string(), Value::String("ok".to_string()));
        value
    })
}

fn uri_for_path(path: &Path) -> Uri {
    let abs = AbsPathBuf::try_from(path.to_path_buf()).expect("abs path");
    path_to_file_uri(&abs)
        .expect("file uri")
        .parse()
        .expect("lsp uri")
}

fn wait_for_semantic_search_indexing(
    stdin: &mut impl std::io::Write,
    stdout: &mut impl std::io::BufRead,
) -> u64 {
    // The workspace is tiny (two files), so indexing should finish quickly. Still, poll with a
    // bounded timeout to keep this deterministic across platforms/CI.
    for attempt in 0..100u64 {
        let id = 1000 + attempt as i64;
        write_jsonrpc_message(
            stdin,
            &jsonrpc_request(
                empty_object(),
                id,
                nova_lsp::SEMANTIC_SEARCH_INDEX_STATUS_METHOD,
            ),
        );
        let resp = read_response_with_id(stdout, id);
        if resp
            .get("result")
            .and_then(|r| r.get("done"))
            .and_then(|v| v.as_bool())
            == Some(true)
        {
            return resp
                .pointer("/result/currentRunId")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for semantic search workspace indexing");
}

#[test]
fn stdio_server_semantic_search_indexes_non_open_workspace_files_for_ai_context() {
    let _lock = stdio_server_lock();

    let mock_server = MockServer::start();
    let mock = mock_server.mock(|when, then| {
        when.method(POST)
            .path("/complete")
            // The related code section should include content from Helper.java even though it was
            // never opened by the client.
            .body_contains("class Helper");
        then.status(200).json_body(completion_response_ok());
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
    std::fs::write(&config_path, &config).expect("write config");

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

    // 1) initialize with a workspace root so workspace indexing can walk it.
    write_jsonrpc_message(
        &mut stdin,
        &initialize_request_with_root_uri(1, root_uri.to_string()),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &initialized_notification());

    // 2) Wait for the background workspace semantic-search index to be populated.
    let _run_id = wait_for_semantic_search_indexing(&mut stdin, &mut stdout);

    // 3) Open only the focal document (Main.java). Helper.java stays closed.
    write_jsonrpc_message(
        &mut stdin,
        &did_open_notification(main_uri.clone(), "java", 1, main_text),
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
        &jsonrpc_request(
            CodeActionParams {
                text_document: TextDocumentIdentifier { uri: main_uri },
                range,
                context: CodeActionContext {
                    diagnostics: vec![Diagnostic::new_simple(
                        range,
                        "cannot find symbol".to_string(),
                    )],
                    ..CodeActionContext::default()
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: lsp_types::PartialResultParams::default(),
            },
            2,
            "textDocument/codeAction",
        ),
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
    let arguments = args
        .as_array()
        .cloned()
        .expect("command.arguments must be an array");
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            ExecuteCommandParams {
                command: cmd.to_string(),
                arguments,
                work_done_progress_params: WorkDoneProgressParams::default(),
            },
            3,
            "workspace/executeCommand",
        ),
    );
    let exec_resp = read_response_with_id(&mut stdout, 3);
    assert_eq!(exec_resp.get("result").and_then(|v| v.as_str()), Some("ok"));
    mock.assert_hits(1);

    // 6) shutdown + exit
    write_jsonrpc_message(&mut stdin, &shutdown_request(4));
    let _shutdown_resp = read_response_with_id(&mut stdout, 4);

    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_semantic_search_reindexes_after_config_reload() {
    let _lock = stdio_server_lock();

    let mock_server = MockServer::start();
    let _mock = mock_server.mock(|when, then| {
        when.method(POST).path("/complete");
        then.status(200).json_body(completion_response_ok());
    });

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    let root_uri = uri_for_path(root);

    let src_dir = root.join("src");
    std::fs::create_dir_all(&src_dir).expect("create src dir");

    let main_path = src_dir.join("Main.java");
    let helper_path = src_dir.join("Helper.java");

    std::fs::write(
        &main_path,
        r#"class Main { void run() { Helper.hello(); } }"#,
    )
    .expect("write Main.java");
    std::fs::write(&helper_path, r#"class Helper { static void hello() { } }"#)
        .expect("write Helper.java");

    let config_path = root.join("nova.config.toml");
    let config_uri = uri_for_path(&config_path);
    let endpoint = format!("{}/complete", mock_server.base_url());

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
        endpoint = endpoint
    );
    std::fs::write(&config_path, &config).expect("write config");

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
        &initialize_request_with_root_uri(1, root_uri.to_string()),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &initialized_notification());

    let run1 = wait_for_semantic_search_indexing(&mut stdin, &mut stdout);

    // Rewrite the config to trigger a reload via didChangeWatchedFiles.
    std::fs::write(&config_path, format!("{config}\n")).expect("rewrite config");
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_notification(
            lsp_types::DidChangeWatchedFilesParams {
                changes: vec![FileEvent::new(config_uri, FileChangeType::CHANGED)],
            },
            "workspace/didChangeWatchedFiles",
        ),
    );

    let run2 = wait_for_semantic_search_indexing(&mut stdin, &mut stdout);
    assert!(
        run2 > run1,
        "expected semantic search indexing to restart after config reload (run1={run1}, run2={run2})"
    );

    write_jsonrpc_message(&mut stdin, &shutdown_request(2));
    let _shutdown_resp = read_response_with_id(&mut stdout, 2);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_semantic_search_reindexes_after_workspace_folder_change() {
    let _lock = stdio_server_lock();

    let mock_server = MockServer::start();
    let _mock = mock_server.mock(|when, then| {
        when.method(POST).path("/complete");
        then.status(200).json_body(completion_response_ok());
    });

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    let ws1 = root.join("ws1");
    let ws2 = root.join("ws2");
    std::fs::create_dir_all(ws1.join("src")).expect("create ws1/src");
    std::fs::create_dir_all(ws2.join("src")).expect("create ws2/src");

    std::fs::write(
        ws1.join("src").join("Main.java"),
        r#"class Main { void run() { } }"#,
    )
    .expect("write ws1/Main.java");
    std::fs::write(
        ws2.join("src").join("Other.java"),
        r#"class Other { void run() { } }"#,
    )
    .expect("write ws2/Other.java");

    let config_path = root.join("nova.config.toml");
    let endpoint = format!("{}/complete", mock_server.base_url());
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
        endpoint = endpoint
    );
    std::fs::write(&config_path, &config).expect("write config");

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
        .env_remove("NOVA_AI_AUDIT_LOGGING")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    let ws1_uri = uri_for_path(&ws1);
    let ws2_uri = uri_for_path(&ws2);

    write_jsonrpc_message(
        &mut stdin,
        &initialize_request_with_root_uri(1, ws1_uri.to_string()),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &initialized_notification());

    let run1 = wait_for_semantic_search_indexing(&mut stdin, &mut stdout);

    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_notification(
            lsp_types::DidChangeWorkspaceFoldersParams {
                event: WorkspaceFoldersChangeEvent {
                    added: vec![WorkspaceFolder {
                        uri: ws2_uri,
                        name: "ws2".to_string(),
                    }],
                    removed: vec![WorkspaceFolder {
                        uri: ws1_uri,
                        name: "ws1".to_string(),
                    }],
                },
            },
            "workspace/didChangeWorkspaceFolders",
        ),
    );

    let run2 = wait_for_semantic_search_indexing(&mut stdin, &mut stdout);
    assert!(
        run2 > run1,
        "expected semantic search indexing to restart after workspace folder change (run1={run1}, run2={run2})"
    );

    write_jsonrpc_message(&mut stdin, &shutdown_request(2));
    let _shutdown_resp = read_response_with_id(&mut stdout, 2);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
