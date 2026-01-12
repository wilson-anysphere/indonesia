use lsp_types::Uri;
use nova_core::{path_to_file_uri, AbsPathBuf};
use serde_json::json;
use std::io::BufReader;
use std::process::{Command, Stdio};
use tempfile::TempDir;

use crate::support;
use support::{read_response_with_id, stdio_server_lock, write_jsonrpc_message};

fn uri_for_path(path: &std::path::Path) -> Uri {
    let abs = AbsPathBuf::try_from(path.to_path_buf()).expect("abs path");
    path_to_file_uri(&abs)
        .expect("file uri")
        .parse()
        .expect("lsp uri")
}

fn diagnostic_messages(resp: &serde_json::Value) -> Vec<String> {
    resp.pointer("/result/items")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|item| {
            item.get("message")
                .and_then(|m| m.as_str())
                .map(ToOwned::to_owned)
        })
        .collect()
}

#[test]
fn did_create_delete_files_updates_cached_analysis_state() {
    let _lock = stdio_server_lock();

    let temp = TempDir::new().expect("tempdir");
    let file_path = temp.path().join("Main.java");
    let uri = uri_for_path(&file_path);

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

    // 1) Request diagnostics for a file that doesn't exist. The server should cache "missing".
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/diagnostic",
            "params": { "textDocument": { "uri": uri } }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    assert!(diagnostic_messages(&resp).is_empty());

    // 2) Create the file on disk, but *don't* notify yet. Diagnostics should remain stale.
    let created = r#"class Main {
    void test() {
        bar();
    }
}
"#;
    std::fs::write(&file_path, created).expect("write Main.java");

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "textDocument/diagnostic",
            "params": { "textDocument": { "uri": uri } }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 3);
    assert!(
        diagnostic_messages(&resp).is_empty(),
        "expected cached missing state to stay in effect until didCreateFiles"
    );

    // 3) Notify about file creation; diagnostics should now see the unresolved reference.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "workspace/didCreateFiles",
            "params": { "files": [{ "uri": uri }] }
        }),
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "textDocument/diagnostic",
            "params": { "textDocument": { "uri": uri } }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 4);
    assert!(
        diagnostic_messages(&resp)
            .iter()
            .any(|m| m.contains("Cannot resolve symbol 'bar'")),
        "expected diagnostics to refresh after didCreateFiles, got: {resp:?}"
    );

    // 4) Delete on disk without notifying; diagnostics should still use cached content.
    std::fs::remove_file(&file_path).expect("remove Main.java");
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "textDocument/diagnostic",
            "params": { "textDocument": { "uri": uri } }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 5);
    assert!(
        diagnostic_messages(&resp)
            .iter()
            .any(|m| m.contains("Cannot resolve symbol 'bar'")),
        "expected cached contents to remain until didDeleteFiles"
    );

    // 5) Notify about deletion; diagnostics should now treat the file as missing again.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "workspace/didDeleteFiles",
            "params": { "files": [{ "uri": uri }] }
        }),
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "textDocument/diagnostic",
            "params": { "textDocument": { "uri": uri } }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 6);
    assert!(
        diagnostic_messages(&resp).is_empty(),
        "expected didDeleteFiles to mark the file missing, got: {resp:?}"
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 7, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 7);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
