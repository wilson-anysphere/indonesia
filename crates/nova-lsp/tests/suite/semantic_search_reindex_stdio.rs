use serde_json::json;
use std::io::BufReader;
use std::process::{Command, Stdio};

use tempfile::TempDir;

use crate::support::{file_uri_string, read_response_with_id, stdio_server_lock, write_jsonrpc_message, TestAiServer};

#[test]
fn stdio_server_semantic_search_reindex_starts_new_workspace_index_run() {
    let _lock = stdio_server_lock();

    // Provide a well-formed AI config so the stdio server constructs a runtime and can spawn the
    // background semantic-search workspace indexing task.
    let ai_server = TestAiServer::start(json!({ "completion": "ok" }));

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    let root_uri = file_uri_string(root);

    // Create enough files to make indexing take non-trivial time, so we can reliably observe
    // `completedRunId` resetting to `0` for a newly-started run.
    let src_dir = root.join("src");
    std::fs::create_dir_all(&src_dir).expect("create src dir");
    let filler = "x".repeat(4 * 1024); // 4 KiB
    for i in 0..2_000u32 {
        let path = src_dir.join(format!("File{i}.java"));
        std::fs::write(&path, format!("class File{i} {{ String v = \"{filler}\"; }}"))
            .expect("write java file");
    }

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
        endpoint = format!("{}/complete", ai_server.base_url())
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

    // 1) Observe the initial run id (triggered by initialize).
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": nova_lsp::SEMANTIC_SEARCH_INDEX_STATUS_METHOD,
            "params": {}
        }),
    );
    let status_before = read_response_with_id(&mut stdout, 2);
    let run_before = status_before
        .pointer("/result/currentRunId")
        .and_then(|v| v.as_u64())
        .expect("status.currentRunId");
    assert!(
        run_before > 0,
        "expected semantic search indexing to start after initialize; got {status_before:#}"
    );

    // 2) Trigger a reindex with params omitted.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": nova_lsp::SEMANTIC_SEARCH_REINDEX_METHOD
        }),
    );
    let reindex_resp = read_response_with_id(&mut stdout, 3);
    let run_after = reindex_resp
        .pointer("/result/currentRunId")
        .and_then(|v| v.as_u64())
        .expect("reindex.currentRunId");
    let completed_after = reindex_resp
        .pointer("/result/completedRunId")
        .and_then(|v| v.as_u64())
        .expect("reindex.completedRunId");
    assert_ne!(
        run_after, run_before,
        "expected currentRunId to change after reindex; before={run_before} after={run_after} resp={reindex_resp:#}"
    );
    assert_eq!(
        completed_after, 0,
        "expected completedRunId to reset to 0 immediately after reindex; resp={reindex_resp:#}"
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": nova_lsp::SEMANTIC_SEARCH_INDEX_STATUS_METHOD,
            "params": {}
        }),
    );
    let status_after = read_response_with_id(&mut stdout, 4);
    let run_after_status = status_after
        .pointer("/result/currentRunId")
        .and_then(|v| v.as_u64())
        .expect("status.currentRunId");
    let completed_after_status = status_after
        .pointer("/result/completedRunId")
        .and_then(|v| v.as_u64())
        .expect("status.completedRunId");
    assert_eq!(
        run_after_status, run_after,
        "expected indexStatus.currentRunId to match reindex response; status={status_after:#}"
    );
    assert_eq!(
        completed_after_status, 0,
        "expected completedRunId to still be 0 immediately after reindex; status={status_after:#}"
    );

    // 3) Trigger a reindex with explicit `null` params (should be accepted).
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": nova_lsp::SEMANTIC_SEARCH_REINDEX_METHOD,
            "params": null
        }),
    );
    let reindex_resp2 = read_response_with_id(&mut stdout, 5);
    let run_after_2 = reindex_resp2
        .pointer("/result/currentRunId")
        .and_then(|v| v.as_u64())
        .expect("reindex.currentRunId");
    let completed_after_2 = reindex_resp2
        .pointer("/result/completedRunId")
        .and_then(|v| v.as_u64())
        .expect("reindex.completedRunId");
    assert_ne!(
        run_after_2, run_after,
        "expected currentRunId to change after reindex(null); before={run_after} after={run_after_2} resp={reindex_resp2:#}"
    );
    assert_eq!(
        completed_after_2, 0,
        "expected completedRunId to reset to 0 after reindex(null); resp={reindex_resp2:#}"
    );

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

