use crate::support;
use serde_json::json;
use std::io::BufReader;
use std::process::{Command, Stdio};

#[test]
fn stdio_server_enforces_safe_mode_across_custom_endpoints() {
    const SAFE_MODE_MESSAGE: &str = "Nova is running in safe-mode (previous request crashed or timed out). Only `nova/bugReport`, `nova/metrics`, `nova/resetMetrics`, and `nova/safeModeStatus` are available for now.";

    let _guard = support::stdio_server_lock();

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        // Test hook (debug builds only): force safe-mode without relying on a real
        // watchdog timeout/panic.
        .env("NOVA_LSP_TEST_FORCE_SAFE_MODE", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

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

    // Allowlisted endpoints should still succeed.
    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 2, "method": "nova/safeModeStatus", "params": null }),
    );
    let safe_mode_resp = support::read_response_with_id(&mut stdout, 2);
    assert!(
        safe_mode_resp.get("error").is_none(),
        "expected success, got: {safe_mode_resp:?}"
    );
    assert_eq!(
        safe_mode_resp
            .get("result")
            .and_then(|v| v.get("enabled"))
            .and_then(|v| v.as_bool()),
        Some(true)
    );

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 3, "method": "nova/metrics", "params": null }),
    );
    let metrics_resp = support::read_response_with_id(&mut stdout, 3);
    assert!(
        metrics_resp.get("error").is_none(),
        "expected success, got: {metrics_resp:?}"
    );

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 4, "method": "nova/resetMetrics", "params": null }),
    );
    let reset_metrics_resp = support::read_response_with_id(&mut stdout, 4);
    assert!(
        reset_metrics_resp.get("error").is_none(),
        "expected success, got: {reset_metrics_resp:?}"
    );

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 5, "method": "nova/bugReport", "params": null }),
    );
    let bug_report_resp = support::read_response_with_id(&mut stdout, 5);
    assert!(
        bug_report_resp.get("error").is_none(),
        "expected success, got: {bug_report_resp:?}"
    );
    assert!(
        bug_report_resp
            .get("result")
            .and_then(|v| v.get("path"))
            .and_then(|v| v.as_str())
            .is_some(),
        "expected bug report path, got: {bug_report_resp:?}"
    );

    // Non-allowlisted endpoints should be blocked, even if their params are invalid.
    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 6, "method": "nova/memoryStatus", "params": null }),
    );
    let memory_status_resp = support::read_response_with_id(&mut stdout, 6);
    assert_eq!(
        memory_status_resp
            .get("error")
            .and_then(|v| v.get("code"))
            .and_then(|v| v.as_i64()),
        Some(-32603),
        "expected safe-mode error, got: {memory_status_resp:?}"
    );
    assert_eq!(
        memory_status_resp
            .get("error")
            .and_then(|v| v.get("message"))
            .and_then(|v| v.as_str()),
        Some(SAFE_MODE_MESSAGE)
    );

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 7, "method": nova_lsp::SEMANTIC_SEARCH_INDEX_STATUS_METHOD, "params": null }),
    );
    let index_status_resp = support::read_response_with_id(&mut stdout, 7);
    assert_eq!(
        index_status_resp
            .get("error")
            .and_then(|v| v.get("code"))
            .and_then(|v| v.as_i64()),
        Some(-32603),
        "expected safe-mode error, got: {index_status_resp:?}"
    );
    assert_eq!(
        index_status_resp
            .get("error")
            .and_then(|v| v.get("message"))
            .and_then(|v| v.as_str()),
        Some(SAFE_MODE_MESSAGE)
    );

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 8, "method": "nova/extensions/status", "params": null }),
    );
    let extensions_status_resp = support::read_response_with_id(&mut stdout, 8);
    assert_eq!(
        extensions_status_resp
            .get("error")
            .and_then(|v| v.get("code"))
            .and_then(|v| v.as_i64()),
        Some(-32603),
        "expected safe-mode error, got: {extensions_status_resp:?}"
    );
    assert_eq!(
        extensions_status_resp
            .get("error")
            .and_then(|v| v.get("message"))
            .and_then(|v| v.as_str()),
        Some(SAFE_MODE_MESSAGE)
    );

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 9, "method": "nova/extensions/navigation", "params": null }),
    );
    let extensions_navigation_resp = support::read_response_with_id(&mut stdout, 9);
    assert_eq!(
        extensions_navigation_resp
            .get("error")
            .and_then(|v| v.get("code"))
            .and_then(|v| v.as_i64()),
        Some(-32603),
        "expected safe-mode error, got: {extensions_navigation_resp:?}"
    );
    assert_eq!(
        extensions_navigation_resp
            .get("error")
            .and_then(|v| v.get("message"))
            .and_then(|v| v.as_str()),
        Some(SAFE_MODE_MESSAGE)
    );

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 10, "method": "nova/java/organizeImports", "params": null }),
    );
    let organize_imports_resp = support::read_response_with_id(&mut stdout, 10);
    assert_eq!(
        organize_imports_resp
            .get("error")
            .and_then(|v| v.get("code"))
            .and_then(|v| v.as_i64()),
        Some(-32603),
        "expected safe-mode error, got: {organize_imports_resp:?}"
    );
    assert_eq!(
        organize_imports_resp
            .get("error")
            .and_then(|v| v.get("message"))
            .and_then(|v| v.as_str()),
        Some(SAFE_MODE_MESSAGE)
    );

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 11, "method": "nova/ai/explainError", "params": null }),
    );
    let explain_error_resp = support::read_response_with_id(&mut stdout, 11);
    assert_eq!(
        explain_error_resp
            .get("error")
            .and_then(|v| v.get("code"))
            .and_then(|v| v.as_i64()),
        Some(-32603),
        "expected safe-mode error, got: {explain_error_resp:?}"
    );
    assert_eq!(
        explain_error_resp
            .get("error")
            .and_then(|v| v.get("message"))
            .and_then(|v| v.as_str()),
        Some(SAFE_MODE_MESSAGE)
    );

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 12, "method": "nova/completion/more", "params": null }),
    );
    let completion_more_resp = support::read_response_with_id(&mut stdout, 12);
    assert_eq!(
        completion_more_resp
            .get("error")
            .and_then(|v| v.get("code"))
            .and_then(|v| v.as_i64()),
        Some(-32603),
        "expected safe-mode error, got: {completion_more_resp:?}"
    );
    assert_eq!(
        completion_more_resp
            .get("error")
            .and_then(|v| v.get("message"))
            .and_then(|v| v.as_str()),
        Some(SAFE_MODE_MESSAGE)
    );

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 13, "method": "shutdown" }),
    );
    let _shutdown_resp = support::read_response_with_id(&mut stdout, 13);
    support::write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
