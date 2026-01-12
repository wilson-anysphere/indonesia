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
fn stdio_server_supports_hover_signature_help_and_references() {
    let _lock = stdio_server_lock();

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    let cache_dir = TempDir::new().expect("cache dir");

    let file_path = root.join("Main.java");
    let file_uri = uri_for_path(&file_path);
    let disk_file_path = root.join("OnDisk.java");
    let disk_file_uri = uri_for_path(&disk_file_path);
    let root_uri = uri_for_path(root);

    let source = concat!(
        "class Main {\n",
        "    void foo(int x, String y) {}\n",
        "\n",
        "    void test() {\n",
        "        foo(1, \"a\");\n",
        "        foo(2, \"b\");\n",
        "    }\n",
        "}\n",
    );
    let disk_source = concat!(
        "class OnDisk {\n",
        "    void foo(int x, String y) {}\n",
        "\n",
        "    void test() {\n",
        "        foo(1, \"a\");\n",
        "        foo(2, \"b\");\n",
        "    }\n",
        "}\n",
    );

    std::fs::write(&disk_file_path, disk_source).expect("write on-disk java file");

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
    let caps = initialize_resp
        .get("result")
        .and_then(|v| v.get("capabilities"))
        .expect("initialize capabilities");
    assert_eq!(
        caps.get("referencesProvider").and_then(|v| v.as_bool()),
        Some(true)
    );
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    // open document
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
                    "text": source,
                }
            }
        }),
    );

    // hover on method identifier in declaration
    let hover_offset = source
        .find("foo(int")
        .expect("foo(int in method declaration");
    let hover_pos = utf16_position(source, hover_offset);
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/hover",
            "params": {
                "textDocument": { "uri": file_uri.as_str() },
                "position": { "line": hover_pos.line, "character": hover_pos.character }
            }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let hover_contents = resp
        .pointer("/result/contents/value")
        .and_then(|v| v.as_str())
        .expect("hover contents");
    assert!(
        hover_contents.contains("```java"),
        "expected java code block"
    );
    assert!(
        hover_contents.contains("foo"),
        "expected method name in hover"
    );

    // signature help inside argument list
    let call_offset = source.find("foo(1").expect("foo(1 call") + "foo(".len();
    let call_pos = utf16_position(source, call_offset);
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "textDocument/signatureHelp",
            "params": {
                "textDocument": { "uri": file_uri.as_str() },
                "position": { "line": call_pos.line, "character": call_pos.character }
            }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 3);
    let signatures = resp
        .pointer("/result/signatures")
        .and_then(|v| v.as_array())
        .expect("signatureHelp signatures array");
    assert!(!signatures.is_empty(), "expected non-empty signatures");
    let label = signatures[0]
        .get("label")
        .and_then(|v| v.as_str())
        .expect("signature label");
    assert!(label.contains("foo"), "expected foo in signature label");

    // references on call-site identifier
    let ref_offset = source.find("foo(1").expect("foo(1 call");
    let ref_pos = utf16_position(source, ref_offset);
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "textDocument/references",
            "params": {
                "textDocument": { "uri": file_uri.as_str() },
                "position": { "line": ref_pos.line, "character": ref_pos.character },
                "context": { "includeDeclaration": true }
            }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 4);
    let locations = resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("references result array");
    assert!(
        locations.len() >= 2,
        "expected at least 2 reference locations (including declaration)"
    );

    // ---------------------------------------------------------------------
    // Disk-backed (non-overlay) requests
    // ---------------------------------------------------------------------

    // hover on method identifier in declaration
    let hover_offset = disk_source
        .find("foo(int")
        .expect("foo(int in method declaration");
    let hover_pos = utf16_position(disk_source, hover_offset);
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "textDocument/hover",
            "params": {
                "textDocument": { "uri": disk_file_uri.as_str() },
                "position": { "line": hover_pos.line, "character": hover_pos.character }
            }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 5);
    let hover_contents = resp
        .pointer("/result/contents/value")
        .and_then(|v| v.as_str())
        .expect("hover contents");
    assert!(
        hover_contents.contains("```java"),
        "expected java code block"
    );
    assert!(
        hover_contents.contains("foo"),
        "expected method name in hover"
    );

    // signature help inside argument list
    let call_offset = disk_source.find("foo(1").expect("foo(1 call") + "foo(".len();
    let call_pos = utf16_position(disk_source, call_offset);
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "textDocument/signatureHelp",
            "params": {
                "textDocument": { "uri": disk_file_uri.as_str() },
                "position": { "line": call_pos.line, "character": call_pos.character }
            }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 6);
    let signatures = resp
        .pointer("/result/signatures")
        .and_then(|v| v.as_array())
        .expect("signatureHelp signatures array");
    assert!(!signatures.is_empty(), "expected non-empty signatures");
    let label = signatures[0]
        .get("label")
        .and_then(|v| v.as_str())
        .expect("signature label");
    assert!(label.contains("foo"), "expected foo in signature label");

    // references on call-site identifier
    let ref_offset = disk_source.find("foo(1").expect("foo(1 call");
    let ref_pos = utf16_position(disk_source, ref_offset);
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "textDocument/references",
            "params": {
                "textDocument": { "uri": disk_file_uri.as_str() },
                "position": { "line": ref_pos.line, "character": ref_pos.character },
                "context": { "includeDeclaration": true }
            }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 7);
    let locations = resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("references result array");
    assert!(
        locations.len() >= 2,
        "expected at least 2 reference locations (including declaration)"
    );

    // shutdown + exit
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 8, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 8);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
