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
fn stdio_server_supports_text_document_references_for_open_documents() {
    let _lock = stdio_server_lock();

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    let root_uri = uri_for_path(root);

    let cache_dir = TempDir::new().expect("cache dir");

    let foo_path = root.join("Foo.java");
    let main_path = root.join("Main.java");

    let foo_uri = uri_for_path(&foo_path);
    let main_uri = uri_for_path(&main_path);

    let foo_text = concat!("public class Foo {\n", "    public void foo() {}\n", "}\n",);

    let main_text = concat!(
        "public class Main {\n",
        "    public void test() {\n",
        "        Foo foo = new Foo();\n",
        "        foo.foo();\n",
        "    }\n",
        "}\n",
    );

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
    assert_eq!(
        initialize_resp
            .pointer("/result/capabilities/referencesProvider")
            .and_then(|v| v.as_bool()),
        Some(true),
        "server must advertise referencesProvider"
    );
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    for (uri, text) in [(&foo_uri, foo_text), (&main_uri, main_text)] {
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
                        "text": text,
                    }
                }
            }),
        );
    }

    let foo_def_offset = foo_text.find("void foo").expect("foo decl") + "void ".len();
    let foo_def_pos = utf16_position(foo_text, foo_def_offset);
    let foo_usage_offset = main_text.find(".foo()").expect("foo usage") + ".".len();
    let foo_usage_pos = utf16_position(main_text, foo_usage_offset);

    // 1) includeDeclaration: false (should still return cross-file usage).
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/references",
            "params": {
                "textDocument": { "uri": foo_uri.as_str() },
                "position": { "line": foo_def_pos.line, "character": foo_def_pos.character },
                "context": { "includeDeclaration": false }
            }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let locations = resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("references result array");

    assert!(
        locations.iter().any(|loc| {
            loc.get("uri").and_then(|v| v.as_str()) == Some(main_uri.as_str())
                && loc.pointer("/range/start/line").and_then(|v| v.as_u64())
                    == Some(foo_usage_pos.line as u64)
                && loc
                    .pointer("/range/start/character")
                    .and_then(|v| v.as_u64())
                    == Some(foo_usage_pos.character as u64)
        }),
        "expected references to include usage in Main.java; got {locations:?}"
    );

    assert!(
        !locations.iter().any(|loc| {
            loc.get("uri").and_then(|v| v.as_str()) == Some(foo_uri.as_str())
                && loc.pointer("/range/start/line").and_then(|v| v.as_u64())
                    == Some(foo_def_pos.line as u64)
                && loc
                    .pointer("/range/start/character")
                    .and_then(|v| v.as_u64())
                    == Some(foo_def_pos.character as u64)
        }),
        "expected includeDeclaration=false to omit foo declaration; got {locations:?}"
    );

    // 2) includeDeclaration: true (should include declaration).
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "textDocument/references",
            "params": {
                "textDocument": { "uri": foo_uri.as_str() },
                "position": { "line": foo_def_pos.line, "character": foo_def_pos.character },
                "context": { "includeDeclaration": true }
            }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 3);
    let locations = resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("references result array");
    assert!(
        locations.iter().any(|loc| {
            loc.get("uri").and_then(|v| v.as_str()) == Some(foo_uri.as_str())
                && loc.pointer("/range/start/line").and_then(|v| v.as_u64())
                    == Some(foo_def_pos.line as u64)
                && loc
                    .pointer("/range/start/character")
                    .and_then(|v| v.as_u64())
                    == Some(foo_def_pos.character as u64)
        }),
        "expected includeDeclaration=true to include foo declaration; got {locations:?}"
    );

    // shutdown + exit
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 4, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 4);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
