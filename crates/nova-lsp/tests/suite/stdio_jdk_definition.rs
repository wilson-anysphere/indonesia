use nova_core::{path_to_file_uri, AbsPathBuf, LineIndex, TextSize};
use serde_json::json;
use std::io::BufReader;
use std::path::{Path, PathBuf};
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
fn stdio_definition_into_jdk_returns_decompiled_uri_and_is_readable() {
    // Point JDK discovery at the tiny fake JDK shipped in this repository.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fake_jdk_root = manifest_dir.join("../nova-jdk/testdata/fake-jdk");
    assert!(
        fake_jdk_root.is_dir(),
        "expected fake JDK at {}",
        fake_jdk_root.display()
    );

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();

    let main_path = root.join("Main.java");
    let main_uri = uri_for_path(&main_path);
    let text = "class Main { void m() { String s = \"\"; } }";
    std::fs::write(&main_path, text).expect("write Main.java");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .env("JAVA_HOME", &fake_jdk_root)
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

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": main_uri.as_str(),
                    "languageId": "java",
                    "version": 1,
                    "text": text,
                }
            }
        }),
    );

    let offset = text.find("String").expect("String token exists");
    let position = utf16_position(text, offset);
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/definition",
            "params": {
                "textDocument": { "uri": main_uri.as_str() },
                "position": { "line": position.line, "character": position.character }
            }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let location = resp.get("result").expect("definition result");
    let Some(uri) = location.get("uri").and_then(|v| v.as_str()) else {
        panic!("expected definition uri, got: {resp:?}");
    };

    assert!(
        uri.starts_with("nova:///decompiled/"),
        "expected decompiled uri, got: {uri}"
    );
    assert!(
        nova_decompile::parse_decompiled_uri(uri).is_some(),
        "expected decompiled uri to be canonical/parseable, got: {uri}"
    );

    // Ask the server to parse the decompiled buffer via documentSymbol. This exercises that the
    // VFS can load the virtual document returned from definition.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "textDocument/documentSymbol",
            "params": { "textDocument": { "uri": uri } }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 3);
    let symbols = resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("documentSymbol result array");
    assert!(
        !symbols.is_empty(),
        "expected decompiled document to return symbols, got: {resp:?}"
    );

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
