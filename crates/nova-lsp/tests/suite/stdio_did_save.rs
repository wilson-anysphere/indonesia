use serde_json::json;
use std::io::BufReader;
use std::process::{Command, Stdio};

use crate::support;
use support::{read_response_with_id, write_jsonrpc_message};

#[test]
fn stdio_server_supports_did_save_and_updates_open_document_contents() {
    let _lock = support::stdio_server_lock();

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
            .pointer("/result/capabilities/textDocumentSync/save/includeText")
            .and_then(|v| v.as_bool()),
        Some(false),
        "expected textDocumentSync.save support with includeText=false, got: {initialize_resp:#}"
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    let uri = "file:///test/Foo.java";
    let opened_text = "public class Foo {\n}\n";
    let saved_text = "public class Foo {\n    void bar() {}\n}\n";

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
                    "text": opened_text,
                }
            }
        }),
    );

    // Use didSave to update the file contents without sending didChange. This exercises the
    // server's ability to accept save notifications with included text.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didSave",
            "params": {
                "textDocument": { "uri": uri },
                "text": saved_text,
            }
        }),
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/documentSymbol",
            "params": { "textDocument": { "uri": uri } }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let results = resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("documentSymbol result array");

    let foo = results
        .iter()
        .find(|value| value.get("name").and_then(|v| v.as_str()) == Some("Foo"))
        .expect("expected Foo symbol");
    let children = foo
        .get("children")
        .and_then(|v| v.as_array())
        .expect("Foo should have children");
    assert!(
        children
            .iter()
            .any(|value| value.get("name").and_then(|v| v.as_str()) == Some("bar")),
        "expected Foo to contain bar() method after didSave update, got: {resp:#}"
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
