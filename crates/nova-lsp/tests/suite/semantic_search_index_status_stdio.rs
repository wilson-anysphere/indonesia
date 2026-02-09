use serde_json::json;
use std::io::BufReader;
use std::process::{Command, Stdio};

use crate::support::{read_response_with_id, stdio_server_lock, write_jsonrpc_message};

#[test]
fn stdio_server_index_status_reports_disabled_when_semantic_search_disabled_in_config() {
    let _lock = stdio_server_lock();

    let temp = tempfile::TempDir::new().expect("tempdir");
    let config_path = temp.path().join("nova.config.toml");
    std::fs::write(
        &config_path,
        r#"
[ai]
enabled = false

[ai.features]
semantic_search = false
"#,
    )
    .expect("write config");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        // Avoid inheriting legacy AI env config (NOVA_AI_*) that would override the file.
        .env_remove("NOVA_AI_PROVIDER")
        .env_remove("NOVA_AI_ENDPOINT")
        .env_remove("NOVA_AI_MODEL")
        .env_remove("NOVA_AI_API_KEY")
        .env_remove("NOVA_AI_AUDIT_LOGGING")
        // Ensure a developer's environment doesn't override the test config.
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
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
    let initialize_resp = read_response_with_id(&mut stdout, 1);

    let requests = initialize_resp
        .pointer("/result/capabilities/experimental/nova/requests")
        .and_then(|v| v.as_array())
        .expect("initializeResult.capabilities.experimental.nova.requests must be an array");
    assert!(
        requests
            .iter()
            .filter_map(|v| v.as_str())
            .any(|m| m == nova_lsp::SEMANTIC_SEARCH_INDEX_STATUS_METHOD),
        "expected {} to be advertised in experimental.nova.requests; got {requests:?}",
        nova_lsp::SEMANTIC_SEARCH_INDEX_STATUS_METHOD
    );
    assert!(
        requests
            .iter()
            .filter_map(|v| v.as_str())
            .any(|m| m == nova_lsp::SEMANTIC_SEARCH_REINDEX_METHOD),
        "expected {} to be advertised in experimental.nova.requests; got {requests:?}",
        nova_lsp::SEMANTIC_SEARCH_REINDEX_METHOD
    );

    let notifications = initialize_resp
        .pointer("/result/capabilities/experimental/nova/notifications")
        .and_then(|v| v.as_array())
        .expect("initializeResult.capabilities.experimental.nova.notifications must be an array");
    assert!(
        notifications
            .iter()
            .filter_map(|v| v.as_str())
            .any(|m| m == nova_lsp::WORKSPACE_RENAME_PATH_NOTIFICATION),
        "expected {} to be advertised in experimental.nova.notifications; got {notifications:?}",
        nova_lsp::WORKSPACE_RENAME_PATH_NOTIFICATION
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": nova_lsp::SEMANTIC_SEARCH_INDEX_STATUS_METHOD,
            "params": {}
        }),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let result = resp.get("result").cloned().expect("result");

    assert_eq!(
        result.get("enabled").and_then(|v| v.as_bool()),
        Some(false),
        "expected result.enabled=false; got {result:#}"
    );
    assert_eq!(
        result.get("reason").and_then(|v| v.as_str()),
        Some("disabled"),
        "expected result.reason=\"disabled\"; got {result:#}"
    );

    assert!(
        result
            .get("currentRunId")
            .and_then(|v| v.as_u64())
            .is_some(),
        "expected result.currentRunId to be a number; got {result:#}"
    );
    assert!(
        result
            .get("completedRunId")
            .and_then(|v| v.as_u64())
            .is_some(),
        "expected result.completedRunId to be a number; got {result:#}"
    );
    assert!(
        result.get("done").and_then(|v| v.as_bool()).is_some(),
        "expected result.done to be a bool; got {result:#}"
    );
    assert!(
        result
            .get("indexedFiles")
            .and_then(|v| v.as_u64())
            .is_some(),
        "expected result.indexedFiles to be a number; got {result:#}"
    );
    assert!(
        result
            .get("indexedBytes")
            .and_then(|v| v.as_u64())
            .is_some(),
        "expected result.indexedBytes to be a number; got {result:#}"
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
fn stdio_server_index_status_reports_missing_workspace_root_when_enabled_without_root() {
    let _lock = stdio_server_lock();

    let temp = tempfile::TempDir::new().expect("tempdir");
    let config_path = temp.path().join("nova.config.toml");
    std::fs::write(
        &config_path,
        r#"
[ai]
enabled = true

[ai.features]
semantic_search = true
"#,
    )
    .expect("write config");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        // Avoid inheriting legacy AI env config (NOVA_AI_*) that would override the file.
        .env_remove("NOVA_AI_PROVIDER")
        .env_remove("NOVA_AI_ENDPOINT")
        .env_remove("NOVA_AI_MODEL")
        .env_remove("NOVA_AI_API_KEY")
        .env_remove("NOVA_AI_AUDIT_LOGGING")
        // Ensure a developer's environment doesn't disable AI for this test.
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    // Initialize without a workspace root; this should prevent workspace indexing from starting.
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

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": nova_lsp::SEMANTIC_SEARCH_INDEX_STATUS_METHOD,
            "params": {}
        }),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let result = resp.get("result").cloned().expect("result");

    assert_eq!(
        result.get("enabled").and_then(|v| v.as_bool()),
        Some(true),
        "expected result.enabled=true; got {result:#}"
    );
    assert_eq!(
        result.get("reason").and_then(|v| v.as_str()),
        Some("missing_workspace_root"),
        "expected result.reason=\"missing_workspace_root\"; got {result:#}"
    );

    assert_eq!(
        result.get("currentRunId").and_then(|v| v.as_u64()),
        Some(0),
        "expected currentRunId to remain 0 without workspace root; got {result:#}"
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
