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

#[test]
fn stdio_workspace_folder_change_restarts_semantic_search_workspace_indexing() {
    let _lock = stdio_server_lock();

    let ai_server = TestAiServer::start(json!({ "completion": "ok" }));

    let temp = TempDir::new().expect("tempdir");
    let root1 = temp.path().join("ws1");
    let root2 = temp.path().join("ws2");
    std::fs::create_dir_all(&root1).expect("create ws1");
    std::fs::create_dir_all(&root2).expect("create ws2");

    std::fs::write(root1.join("Foo.java"), "class Foo {}").expect("write Foo.java");
    std::fs::write(root2.join("Bar.java"), "class Bar {}").expect("write Bar.java");

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

