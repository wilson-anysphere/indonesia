use nova_core::{path_to_file_uri, AbsPathBuf, LineIndex, TextSize};
use serde_json::json;
use std::io::BufReader;
use std::path::Path;
use std::process::{Command, Stdio};
use tempfile::TempDir;

use crate::support::{read_response_with_id, stdio_server_lock, write_jsonrpc_message};

fn uri_for_path(path: &Path) -> String {
    let abs = AbsPathBuf::try_from(path.to_path_buf()).expect("abs path");
    path_to_file_uri(&abs).expect("file uri")
}

fn utf16_position(text: &str, offset: usize) -> nova_core::Position {
    let index = LineIndex::new(text);
    let offset = TextSize::from(u32::try_from(offset).expect("offset fits in u32"));
    index.position(text, offset)
}

#[test]
fn stdio_server_handles_call_and_type_hierarchy_requests() {
    let _lock = stdio_server_lock();

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    let file_path = root.join("Hierarchy.java");
    let uri = uri_for_path(&file_path);

    let text = concat!(
        "class A {\n",
        "}\n",
        "\n",
        "class B extends A {\n",
        "    void foo() {\n",
        "        bar();\n",
        "    }\n",
        "\n",
        "    void bar() {}\n",
        "}\n",
    );

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
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
            "params": { "capabilities": {} }
        }),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
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
                    "uri": uri.as_str(),
                    "languageId": "java",
                    "version": 1,
                    "text": text,
                }
            }
        }),
    );

    // 1) prepareCallHierarchy at `foo`.
    let foo_offset = text.find("foo").expect("foo method");
    let foo_pos = utf16_position(text, foo_offset);
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/prepareCallHierarchy",
            "params": {
                "textDocument": { "uri": uri.as_str() },
                "position": { "line": foo_pos.line, "character": foo_pos.character },
            }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let items = resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("prepareCallHierarchy result array");
    assert!(
        !items.is_empty(),
        "expected prepareCallHierarchy to return at least one item: {resp:#}"
    );
    assert_eq!(items[0].get("name").and_then(|v| v.as_str()), Some("foo"));
    let foo_item = items[0].clone();

    // 2) outgoingCalls for `foo` should include `bar`.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "callHierarchy/outgoingCalls",
            "params": { "item": foo_item }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 3);
    let outgoing = resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("outgoingCalls result array");
    assert!(
        outgoing
            .iter()
            .any(|call| call.pointer("/to/name").and_then(|v| v.as_str()) == Some("bar")),
        "expected outgoing calls to include `bar`, got: {resp:#}"
    );

    // 3) prepareTypeHierarchy at `B`.
    let b_offset = text.find("B extends").expect("class B");
    let b_pos = utf16_position(text, b_offset);
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "textDocument/prepareTypeHierarchy",
            "params": {
                "textDocument": { "uri": uri.as_str() },
                "position": { "line": b_pos.line, "character": b_pos.character },
            }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 4);
    let items = resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("prepareTypeHierarchy result array");
    assert!(
        !items.is_empty(),
        "expected prepareTypeHierarchy to return at least one item: {resp:#}"
    );
    assert_eq!(items[0].get("name").and_then(|v| v.as_str()), Some("B"));
    let b_item = items[0].clone();

    // 4) supertypes for `B` should include `A`.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "typeHierarchy/supertypes",
            "params": { "item": b_item }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 5);
    let supertypes = resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("supertypes result array");
    assert!(
        supertypes
            .iter()
            .any(|item| item.get("name").and_then(|v| v.as_str()) == Some("A")),
        "expected supertypes to include `A`, got: {resp:#}"
    );

    // shutdown + exit
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
