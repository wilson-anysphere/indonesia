use std::io::BufReader;
use std::process::{Command, Stdio};

use crate::support::{
    decode_initialize_result, empty_object, exit_notification, initialize_request_empty,
    initialized_notification, jsonrpc_request, read_response_with_id, shutdown_request,
    stdio_server_lock, write_jsonrpc_message,
};

#[test]
fn stdio_server_advertises_and_handles_semantic_search_index_status_request() {
    let _lock = stdio_server_lock();

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    write_jsonrpc_message(&mut stdin, &initialize_request_empty(1));
    let initialize_resp = read_response_with_id(&mut stdout, 1);

    let init = decode_initialize_result(&initialize_resp);
    let experimental = init
        .capabilities
        .experimental
        .as_ref()
        .expect("initializeResult.capabilities.experimental");
    let nova = experimental
        .get("nova")
        .and_then(|v| v.as_object())
        .expect("initializeResult.capabilities.experimental.nova");
    let requests = nova
        .get("requests")
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

    let notifications = nova
        .get("notifications")
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

    write_jsonrpc_message(&mut stdin, &initialized_notification());

    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            empty_object(),
            2,
            nova_lsp::SEMANTIC_SEARCH_INDEX_STATUS_METHOD,
        ),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let result = resp.get("result").cloned().expect("result");

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

    write_jsonrpc_message(&mut stdin, &shutdown_request(3));
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
