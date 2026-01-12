use serde_json::json;
use std::io::BufReader;
use std::process::{Command, Stdio};

use crate::support::{
    drain_notifications_until_id, read_response_with_id, stdio_server_lock, write_jsonrpc_message,
};

fn find_publish_diagnostics<'a>(
    notifications: &'a [serde_json::Value],
    uri: &str,
) -> Option<&'a serde_json::Value> {
    notifications.iter().find(|msg| {
        msg.get("method").and_then(|v| v.as_str()) == Some("textDocument/publishDiagnostics")
            && msg.pointer("/params/uri").and_then(|v| v.as_str()) == Some(uri)
    })
}

#[test]
fn stdio_publish_diagnostics_for_open_documents() {
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
    let _initialize_resp = read_response_with_id(&mut stdout, 1);

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    let uri = "file:///test/Main.java";
    let broken = "class Main {\n    void test() {\n        bar();\n    }\n}\n";
    let fixed = "class Main {\n    void test() {\n    }\n}\n";

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
                    "text": broken,
                }
            }
        }),
    );

    // Drain messages until we see a response, collecting notifications along the way. The
    // `publishDiagnostics` notification should have been emitted after didOpen.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/diagnostic",
            "params": { "textDocument": { "uri": uri } }
        }),
    );
    let (notifications, _resp) = drain_notifications_until_id(&mut stdout, 2);
    let publish = find_publish_diagnostics(&notifications, uri).unwrap_or_else(|| {
        panic!(
            "expected publishDiagnostics for {uri} after didOpen; got notifications: {notifications:#?}"
        )
    });
    let diagnostics = publish
        .pointer("/params/diagnostics")
        .and_then(|v| v.as_array())
        .expect("publishDiagnostics.params.diagnostics should be an array");
    assert!(
        !diagnostics.is_empty(),
        "expected diagnostics to be non-empty for the broken file; got: {publish:#}"
    );

    let messages = diagnostics
        .iter()
        .filter_map(|d| d.get("message").and_then(|m| m.as_str()))
        .collect::<Vec<_>>();
    assert!(
        messages.iter().any(|m| m.contains("Cannot resolve symbol") && m.contains("bar")),
        "expected a 'Cannot resolve symbol' diagnostic mentioning `bar`, got: {messages:?}"
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didChange",
            "params": {
                "textDocument": { "uri": uri, "version": 2 },
                "contentChanges": [{ "text": fixed }]
            }
        }),
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "textDocument/diagnostic",
            "params": { "textDocument": { "uri": uri } }
        }),
    );
    let (notifications, _resp) = drain_notifications_until_id(&mut stdout, 3);
    let publish = find_publish_diagnostics(&notifications, uri).unwrap_or_else(|| {
        panic!(
            "expected publishDiagnostics for {uri} after didChange; got notifications: {notifications:#?}"
        )
    });
    let diagnostics = publish
        .pointer("/params/diagnostics")
        .and_then(|v| v.as_array())
        .expect("publishDiagnostics.params.diagnostics should be an array");
    assert!(
        diagnostics.is_empty(),
        "expected diagnostics to be cleared after fixing the file; got: {publish:#}"
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didClose",
            "params": { "textDocument": { "uri": uri } }
        }),
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 4, "method": "shutdown" }),
    );
    let (notifications, _shutdown_resp) = drain_notifications_until_id(&mut stdout, 4);
    let publish = find_publish_diagnostics(&notifications, uri).unwrap_or_else(|| {
        panic!(
            "expected publishDiagnostics for {uri} after didClose; got notifications: {notifications:#?}"
        )
    });
    let diagnostics = publish
        .pointer("/params/diagnostics")
        .and_then(|v| v.as_array())
        .expect("publishDiagnostics.params.diagnostics should be an array");
    assert!(
        diagnostics.is_empty(),
        "expected diagnostics to be cleared on didClose; got: {publish:#}"
    );

    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

