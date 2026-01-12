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
fn stdio_server_handles_type_hierarchy_requests() {
    let _lock = stdio_server_lock();

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();

    let file_path = root.join("Hierarchy.java");
    let file_uri = uri_for_path(&file_path);

    let file_text = concat!("class A {}\n", "class B extends A {}\n");

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
    assert!(
        initialize_resp
            .pointer("/result/capabilities/typeHierarchyProvider")
            .is_some(),
        "expected typeHierarchyProvider capability"
    );
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": file_uri,
                    "languageId": "java",
                    "version": 1,
                    "text": file_text,
                }
            }
        }),
    );

    // Prepare type hierarchy for B, then fetch supertypes -> A.
    let b_offset = file_text.find("class B").expect("class B decl exists") + "class ".len();
    let b_pos = utf16_position(file_text, b_offset);
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/prepareTypeHierarchy",
            "params": {
                "textDocument": { "uri": file_uri.as_str() },
                "position": { "line": b_pos.line, "character": b_pos.character },
            }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let items = resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("prepareTypeHierarchy result array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].get("name").and_then(|v| v.as_str()), Some("B"));
    let item_b = items[0].clone();

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "typeHierarchy/supertypes",
            "params": { "item": item_b }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 3);
    let supers = resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("typeHierarchy/supertypes result array");
    assert_eq!(supers.len(), 1);
    assert_eq!(supers[0].get("name").and_then(|v| v.as_str()), Some("A"));

    // Prepare type hierarchy for A, then fetch subtypes -> B.
    let a_offset = file_text.find("class A").expect("class A decl exists") + "class ".len();
    let a_pos = utf16_position(file_text, a_offset);
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "textDocument/prepareTypeHierarchy",
            "params": {
                "textDocument": { "uri": file_uri.as_str() },
                "position": { "line": a_pos.line, "character": a_pos.character },
            }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 4);
    let items = resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("prepareTypeHierarchy result array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].get("name").and_then(|v| v.as_str()), Some("A"));
    let item_a = items[0].clone();

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "typeHierarchy/subtypes",
            "params": { "item": item_a }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 5);
    let subs = resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("typeHierarchy/subtypes result array");
    assert_eq!(subs.len(), 1);
    assert_eq!(subs[0].get("name").and_then(|v| v.as_str()), Some("B"));

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
