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

fn fake_jdk_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fake_jdk_root = manifest_dir.join("../nova-jdk/testdata/fake-jdk");
    assert!(
        fake_jdk_root.is_dir(),
        "expected fake JDK at {}",
        fake_jdk_root.display()
    );
    fake_jdk_root
}

fn run_type_definition_test(text: &str, offset: usize, expected_suffix: &str) {
    let _lock = crate::support::stdio_server_lock();
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();

    let main_path = root.join("Main.java");
    let main_uri = uri_for_path(&main_path);
    std::fs::write(&main_path, text).expect("write Main.java");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .env("JAVA_HOME", fake_jdk_root())
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

    let position = utf16_position(text, offset);
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/typeDefinition",
            "params": {
                "textDocument": { "uri": main_uri.as_str() },
                "position": { "line": position.line, "character": position.character }
            }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let location = resp.get("result").expect("typeDefinition result");
    let Some(uri) = location.get("uri").and_then(|v| v.as_str()) else {
        panic!("expected typeDefinition uri, got: {resp:?}");
    };

    assert!(
        uri.starts_with("nova:///decompiled/"),
        "expected decompiled uri, got: {uri}"
    );
    assert!(
        uri.ends_with(expected_suffix),
        "expected uri to end with {expected_suffix}, got: {uri}"
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

#[test]
fn stdio_type_definition_on_string_variable_returns_decompiled_string_uri() {
    let text = "class Main { void m(){ String s = \"\"; s.toString(); } }";
    let offset = text.find("s.toString").expect("usage exists");
    run_type_definition_test(text, offset, "/java.lang.String.java");
}

#[test]
fn stdio_type_definition_on_imported_list_variable_returns_decompiled_list_uri() {
    let text = concat!(
        "import java.util.List;\n",
        "class Main { void m(){ List l = null; l.toString(); } }\n",
    );
    let offset = text.find("l.toString").expect("usage exists");
    run_type_definition_test(text, offset, "/java.util.List.java");
}
