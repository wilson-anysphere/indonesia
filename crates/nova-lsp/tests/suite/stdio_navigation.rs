use nova_core::{path_to_file_uri, AbsPathBuf, LineIndex, TextSize};
use serde_json::json;
use std::io::BufReader;
use std::path::Path;
use std::process::{Command, Stdio};
use tempfile::TempDir;

use crate::support::{read_response_with_id, write_jsonrpc_message};

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
fn stdio_server_handles_implementation_declaration_and_type_definition_requests() {
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();

    let iface_path = root.join("I.java");
    let impl_path = root.join("C.java");
    let foo_path = root.join("Foo.java");
    let main_path = root.join("Main.java");

    let iface_uri = uri_for_path(&iface_path);
    let impl_uri = uri_for_path(&impl_path);
    let foo_uri = uri_for_path(&foo_path);
    let main_uri = uri_for_path(&main_path);

    let iface_text = "interface I {\n    void foo();\n}\n";
    let impl_text = "class C implements I {\n    public void foo() {}\n}\n";
    let foo_text = "class Foo {}\n";
    let main_text = concat!(
        "class Main {\n",
        "    void test() {\n",
        "        Foo foo = new Foo();\n",
        "        foo.toString();\n",
        "    }\n",
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

    for (uri, text) in [
        (&iface_uri, iface_text),
        (&impl_uri, impl_text),
        (&foo_uri, foo_text),
        (&main_uri, main_text),
    ] {
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

    // 1) implementation: interface method -> implementing method.
    let iface_foo_offset = iface_text.find("foo").expect("foo in interface");
    let iface_foo_pos = utf16_position(iface_text, iface_foo_offset);
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/implementation",
            "params": {
                "textDocument": { "uri": iface_uri.as_str() },
                "position": { "line": iface_foo_pos.line, "character": iface_foo_pos.character },
            }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let locations = resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("implementation result array");
    assert_eq!(locations.len(), 1);
    assert_eq!(
        locations[0].get("uri").and_then(|v| v.as_str()),
        Some(impl_uri.as_str())
    );

    let impl_foo_offset = impl_text.find("foo").expect("foo in impl");
    let impl_foo_pos = utf16_position(impl_text, impl_foo_offset);
    assert_eq!(
        locations[0]
            .pointer("/range/start/line")
            .and_then(|v| v.as_u64()),
        Some(impl_foo_pos.line as u64)
    );
    assert_eq!(
        locations[0]
            .pointer("/range/start/character")
            .and_then(|v| v.as_u64()),
        Some(impl_foo_pos.character as u64)
    );

    // 2) declaration: override -> interface declaration.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "textDocument/declaration",
            "params": {
                "textDocument": { "uri": impl_uri.as_str() },
                "position": { "line": impl_foo_pos.line, "character": impl_foo_pos.character },
            }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 3);
    let loc = resp.get("result").expect("declaration result");
    assert_eq!(
        loc.get("uri").and_then(|v| v.as_str()),
        Some(iface_uri.as_str())
    );
    assert_eq!(
        loc.pointer("/range/start/line").and_then(|v| v.as_u64()),
        Some(iface_foo_pos.line as u64)
    );
    assert_eq!(
        loc.pointer("/range/start/character")
            .and_then(|v| v.as_u64()),
        Some(iface_foo_pos.character as u64)
    );

    // 3) typeDefinition: variable usage -> class definition.
    let usage_offset = main_text.find("foo.toString").expect("foo usage in main");
    let usage_pos = utf16_position(main_text, usage_offset);
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "textDocument/typeDefinition",
            "params": {
                "textDocument": { "uri": main_uri.as_str() },
                "position": { "line": usage_pos.line, "character": usage_pos.character },
            }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 4);
    let loc = resp.get("result").expect("typeDefinition result");
    assert_eq!(
        loc.get("uri").and_then(|v| v.as_str()),
        Some(foo_uri.as_str())
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
