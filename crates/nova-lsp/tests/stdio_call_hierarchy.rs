use nova_core::{path_to_file_uri, AbsPathBuf, LineIndex, TextSize};
use serde_json::json;
use std::io::BufReader;
use std::path::Path;
use std::process::{Command, Stdio};
use tempfile::TempDir;

mod support;
use support::{read_response_with_id, stdio_server_lock, write_jsonrpc_message};

fn uri_for_path(path: &Path) -> String {
    let abs = AbsPathBuf::try_from(path.to_path_buf()).expect("abs path");
    path_to_file_uri(&abs).expect("file uri")
}

fn utf16_position(text: &str, offset: usize) -> nova_core::Position {
    let index = LineIndex::new(text);
    let offset = TextSize::from(u32::try_from(offset).expect("offset fits u32"));
    index.position(text, offset)
}

#[test]
fn stdio_server_supports_call_hierarchy_outgoing_calls() {
    let _lock = stdio_server_lock();

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();

    let cache_dir = TempDir::new().expect("cache dir");

    let file_path = root.join("Foo.java");
    let file_uri = uri_for_path(&file_path);
    let root_uri = uri_for_path(root);

    let text = r#"
        public class Foo {
            void caller() {
                callee();
            }

            void callee() {}
        }
    "#;

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .env("NOVA_CACHE_DIR", cache_dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    // initialize
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "rootUri": root_uri, "capabilities": {} }
        }),
    );
    let initialize_resp = read_response_with_id(&mut stdout, 1);
    assert!(
        initialize_resp
            .pointer("/result/capabilities/callHierarchyProvider")
            .is_some(),
        "expected callHierarchyProvider capability"
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    // didOpen
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": file_uri.as_str(),
                    "languageId": "java",
                    "version": 1,
                    "text": text,
                }
            }
        }),
    );

    // prepareCallHierarchy at the caller method name.
    let caller_offset = text.find("caller").expect("caller method name");
    let pos = utf16_position(text, caller_offset);
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/prepareCallHierarchy",
            "params": {
                "textDocument": { "uri": file_uri.as_str() },
                "position": { "line": pos.line, "character": pos.character },
            }
        }),
    );

    let prepare_resp = read_response_with_id(&mut stdout, 2);
    let items = prepare_resp
        .get("result")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("expected prepareCallHierarchy result array: {prepare_resp:#}"));
    assert!(
        !items.is_empty(),
        "expected non-empty prepareCallHierarchy result: {prepare_resp:#}"
    );
    let item = items[0].clone();

    // outgoingCalls should contain `callee`.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "callHierarchy/outgoingCalls",
            "params": { "item": item }
        }),
    );
    let outgoing_resp = read_response_with_id(&mut stdout, 3);
    let outgoing = outgoing_resp
        .get("result")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("expected outgoingCalls result array: {outgoing_resp:#}"));
    assert!(
        outgoing.iter().any(|value| {
            value
                .pointer("/to/name")
                .and_then(|v| v.as_str())
                .is_some_and(|name| name == "callee")
        }),
        "expected outgoing calls to include callee: {outgoing_resp:#}"
    );

    // incomingCalls for `callee` should include `caller`.
    let callee_item = outgoing
        .iter()
        .find(|value| {
            value
                .pointer("/to/name")
                .and_then(|v| v.as_str())
                .is_some_and(|name| name == "callee")
        })
        .and_then(|value| value.get("to"))
        .cloned()
        .unwrap_or_else(|| panic!("expected outgoingCalls to include callee item: {outgoing_resp:#}"));

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "callHierarchy/incomingCalls",
            "params": { "item": callee_item }
        }),
    );
    let incoming_resp = read_response_with_id(&mut stdout, 4);
    let incoming = incoming_resp
        .get("result")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("expected incomingCalls result array: {incoming_resp:#}"));
    assert!(
        incoming.iter().any(|value| {
            value
                .pointer("/from/name")
                .and_then(|v| v.as_str())
                .is_some_and(|name| name == "caller")
        }),
        "expected incoming calls to include caller: {incoming_resp:#}"
    );

    // shutdown + exit
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 5, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 5);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
