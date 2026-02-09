use nova_core::{path_to_file_uri, AbsPathBuf};
use serde_json::json;
use std::io::BufReader;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;
use tempfile::TempDir;

use crate::support::{
    read_response_with_id, stdio_server_lock, write_jsonrpc_message, TestAiServer,
};

fn uri_for_path(path: &Path) -> String {
    let abs = AbsPathBuf::canonicalize(path).expect("abs path");
    path_to_file_uri(&abs).expect("file uri")
}

fn current_semantic_search_run_id(
    stdin: &mut impl std::io::Write,
    stdout: &mut impl std::io::BufRead,
    id: i64,
) -> u64 {
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
    resp.pointer("/result/currentRunId")
        .and_then(|v| v.as_u64())
        .expect("indexStatus.currentRunId must be a number")
}

fn semantic_search_index_status(
    stdin: &mut impl std::io::Write,
    stdout: &mut impl std::io::BufRead,
    id: i64,
) -> serde_json::Value {
    write_jsonrpc_message(
        stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": nova_lsp::SEMANTIC_SEARCH_INDEX_STATUS_METHOD,
            "params": {}
        }),
    );
    read_response_with_id(stdout, id)
        .get("result")
        .cloned()
        .expect("indexStatus result")
}

fn wait_for_semantic_search_indexing_done(
    stdin: &mut impl std::io::Write,
    stdout: &mut impl std::io::BufRead,
    expected_run_id: u64,
) {
    for attempt in 0..100u64 {
        let id = 5_000 + attempt as i64;
        let status = semantic_search_index_status(stdin, stdout, id);
        let run_id = status.get("currentRunId").and_then(|v| v.as_u64()).unwrap_or(0);
        let done = status.get("done").and_then(|v| v.as_bool()).unwrap_or(false);
        if run_id == expected_run_id && done {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let status = semantic_search_index_status(stdin, stdout, 9_999);
    panic!(
        "timed out waiting for semantic search workspace indexing (expected_run_id={expected_run_id}) status={status:#}"
    );
}

fn semantic_search_query(
    stdin: &mut impl std::io::Write,
    stdout: &mut impl std::io::BufRead,
    id: i64,
    query: &str,
) -> Vec<serde_json::Value> {
    write_jsonrpc_message(
        stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": nova_lsp::SEMANTIC_SEARCH_SEARCH_METHOD,
            "params": { "query": query, "limit": 10 }
        }),
    );
    let resp = read_response_with_id(stdout, id);
    resp.pointer("/result/results")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

#[test]
fn stdio_workspace_folder_add_starts_semantic_search_workspace_indexing_after_initialize_without_root(
) {
    let _lock = stdio_server_lock();

    let ai_server = TestAiServer::start(json!({ "completion": "ok" }));

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path().join("ws1");
    let src_dir = root.join("src");
    std::fs::create_dir_all(&src_dir).expect("create ws1/src");
    std::fs::write(src_dir.join("Foo.java"), "class Foo {}").expect("write Foo.java");

    let config_path = temp.path().join("nova.config.toml");
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
        endpoint = format!("{}/complete", ai_server.base_url())
    );
    std::fs::write(&config_path, config).expect("write config");

    let root_uri = uri_for_path(&root);
    let cache_dir = TempDir::new().expect("cache dir");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        .env("NOVA_CACHE_DIR", cache_dir.path())
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

    // Initialize without a root URI; semantic-search workspace indexing should remain idle until
    // the client adds a workspace folder.
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

    let status_before = semantic_search_index_status(&mut stdin, &mut stdout, 2);
    assert_eq!(
        status_before.get("currentRunId").and_then(|v| v.as_u64()),
        Some(0),
        "expected currentRunId=0 before workspace folder is set; got {status_before:#}"
    );
    assert_eq!(
        status_before.get("reason").and_then(|v| v.as_str()),
        Some("missing_workspace_root"),
        "expected reason=missing_workspace_root before workspace folder is set; got {status_before:#}"
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "workspace/didChangeWorkspaceFolders",
            "params": {
                "event": {
                    "added": [{ "uri": root_uri, "name": "ws1" }],
                    "removed": []
                }
            }
        }),
    );

    let run_id = (0..50)
        .find_map(|attempt| {
            let id = 100 + attempt as i64;
            let run_id = current_semantic_search_run_id(&mut stdin, &mut stdout, id);
            (run_id != 0).then_some(run_id)
        })
        .expect("expected semantic-search workspace indexing to start after workspace folder add");
    assert_ne!(run_id, 0);

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
fn stdio_workspace_folder_change_restarts_semantic_search_workspace_indexing() {
    let _lock = stdio_server_lock();

    let ai_server = TestAiServer::start(json!({ "completion": "ok" }));

    let temp = TempDir::new().expect("tempdir");
    let root1 = temp.path().join("ws1");
    let root2 = temp.path().join("ws2");
    std::fs::create_dir_all(&root1).expect("create ws1");
    std::fs::create_dir_all(&root2).expect("create ws2");

    let foo_text = r#"class Foo { String token = "fooToken"; }"#;
    let bar_text = r#"class Bar { String token = "barToken"; }"#;
    std::fs::write(root1.join("Foo.java"), foo_text).expect("write Foo.java");
    std::fs::write(root2.join("Bar.java"), bar_text).expect("write Bar.java");

    let config_path = temp.path().join("nova.config.toml");
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
        endpoint = format!("{}/complete", ai_server.base_url())
    );
    std::fs::write(&config_path, config).expect("write config");

    let root1_uri = uri_for_path(&root1);
    let root2_uri = uri_for_path(&root2);

    let cache_dir = TempDir::new().expect("cache dir");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        .env("NOVA_CACHE_DIR", cache_dir.path())
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
            "params": { "rootUri": root1_uri.clone(), "capabilities": {} }
        }),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    let run_id_1 = (0..20)
        .find_map(|attempt| {
            let id = 10 + attempt as i64;
            let run_id = current_semantic_search_run_id(&mut stdin, &mut stdout, id);
            (run_id != 0).then_some(run_id)
        })
        .expect("expected semantic-search workspace indexing to start for initial root");

    wait_for_semantic_search_indexing_done(&mut stdin, &mut stdout, run_id_1);

    let results_before = semantic_search_query(&mut stdin, &mut stdout, 50, "fooToken");
    assert!(
        results_before.iter().any(|result| {
            result.get("path").and_then(|v| v.as_str()) == Some("Foo.java")
                && result
                    .get("snippet")
                    .and_then(|v| v.as_str())
                    .is_some_and(|s| s.contains("fooToken"))
        }),
        "expected Foo.java to be indexed under initial root; got {results_before:#?}"
    );

    // Switch to root2 and expect the server to restart semantic-search workspace indexing.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "workspace/didChangeWorkspaceFolders",
            "params": {
                "event": {
                    "added": [{ "uri": root2_uri.clone(), "name": "ws2" }],
                    "removed": [{ "uri": root1_uri, "name": "ws1" }]
                }
            }
        }),
    );

    let run_id_2 = (0..100)
        .find_map(|attempt| {
            let id = 100 + attempt as i64;
            let run_id = current_semantic_search_run_id(&mut stdin, &mut stdout, id);
            (run_id != 0 && run_id != run_id_1).then_some(run_id)
        })
        .unwrap_or_else(|| {
            std::thread::sleep(Duration::from_millis(50));
            let run_id = current_semantic_search_run_id(&mut stdin, &mut stdout, 999);
            panic!(
                "expected semantic-search run id to change after workspace folder change \
                 (run_id_1={run_id_1}, current={run_id})"
            );
        });

    assert_ne!(run_id_2, 0);
    assert_ne!(run_id_2, run_id_1);

    wait_for_semantic_search_indexing_done(&mut stdin, &mut stdout, run_id_2);

    let results_after = semantic_search_query(&mut stdin, &mut stdout, 51, "barToken");
    assert!(
        results_after.iter().any(|result| {
            result.get("path").and_then(|v| v.as_str()) == Some("Bar.java")
                && result
                    .get("snippet")
                    .and_then(|v| v.as_str())
                    .is_some_and(|s| s.contains("barToken"))
        }),
        "expected Bar.java to be indexed after root switch; got {results_after:#?}"
    );

    let results_old = semantic_search_query(&mut stdin, &mut stdout, 52, "fooToken");
    assert!(
        results_old.iter().all(|result| {
            let path = result.get("path").and_then(|v| v.as_str()).unwrap_or("");
            !path.ends_with("Foo.java")
        }),
        "expected old workspace results to be cleared after root switch; got {results_old:#?}"
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 2, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 2);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_workspace_folder_remove_resets_semantic_search_workspace_index_state() {
    let _lock = stdio_server_lock();

    let ai_server = TestAiServer::start(json!({ "completion": "ok" }));

    let temp = TempDir::new().expect("tempdir");
    let root1 = temp.path().join("ws1");
    std::fs::create_dir_all(&root1).expect("create ws1");
    std::fs::write(root1.join("Foo.java"), "class Foo {}").expect("write Foo.java");

    let config_path = temp.path().join("nova.config.toml");
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
        endpoint = format!("{}/complete", ai_server.base_url())
    );
    std::fs::write(&config_path, config).expect("write config");

    let root1_uri = uri_for_path(&root1);
    let cache_dir = TempDir::new().expect("cache dir");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        .env("NOVA_CACHE_DIR", cache_dir.path())
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
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "rootUri": root1_uri.clone(), "capabilities": {} }
        }),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    // Ensure indexing run started for the initial root.
    let run_id_1 = (0..20)
        .find_map(|attempt| {
            let id = 10 + attempt as i64;
            let run_id = current_semantic_search_run_id(&mut stdin, &mut stdout, id);
            (run_id != 0).then_some(run_id)
        })
        .expect("expected semantic-search workspace indexing to start for initial root");
    assert_ne!(run_id_1, 0);

    // Remove the current workspace folder. The server should clear the active project root and
    // reset workspace indexing state so indexStatus reports `missing_workspace_root`.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "workspace/didChangeWorkspaceFolders",
            "params": {
                "event": {
                    "added": [],
                    "removed": [{ "uri": root1_uri, "name": "ws1" }]
                }
            }
        }),
    );

    let status = semantic_search_index_status(&mut stdin, &mut stdout, 2);
    assert_eq!(
        status.get("enabled").and_then(|v| v.as_bool()),
        Some(true),
        "expected indexStatus.enabled=true; got {status:#}"
    );
    assert_eq!(
        status.get("reason").and_then(|v| v.as_str()),
        Some("missing_workspace_root"),
        "expected indexStatus.reason=\"missing_workspace_root\"; got {status:#}"
    );
    assert_eq!(
        status.get("currentRunId").and_then(|v| v.as_u64()),
        Some(0),
        "expected indexStatus.currentRunId to reset to 0; got {status:#}"
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
