use serde_json::json;
use std::io::BufReader;
use std::process::{Command, Stdio};

use crate::support::{read_response_with_id, stdio_server_lock, write_jsonrpc_message};

#[test]
fn stdio_server_handles_will_save_notification() {
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
    assert_eq!(
        initialize_resp
            .pointer("/result/capabilities/textDocumentSync/willSave")
            .and_then(|v| v.as_bool()),
        Some(true),
        "expected initializeResult.capabilities.textDocumentSync.willSave=true, got: {initialize_resp:?}"
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    let uri = "file:///test/Foo.java";
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
                    "text": "class Foo{}\n"
                }
            }
        }),
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/willSave",
            "params": {
                "textDocument": { "uri": uri },
                "reason": 1
            }
        }),
    );

    // Verify the server remains responsive after the notification by issuing a lightweight request.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/metrics",
            "params": null
        }),
    );
    let metrics_resp = read_response_with_id(&mut stdout, 2);
    assert!(
        metrics_resp.get("result").is_some(),
        "expected nova/metrics to return a result, got: {metrics_resp:?}"
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
