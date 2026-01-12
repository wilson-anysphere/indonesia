use lsp_types::{Position, Range, Uri};
use nova_core::{path_to_file_uri, AbsPathBuf};
use serde_json::json;
use std::io::BufReader;
use std::process::{Command, Stdio};
use tempfile::TempDir;

use crate::support::{read_response_with_id, write_jsonrpc_message};

fn uri_for_path(path: &std::path::Path) -> Uri {
    let abs = AbsPathBuf::try_from(path.to_path_buf()).expect("abs path");
    path_to_file_uri(&abs)
        .expect("file uri")
        .parse()
        .expect("lsp uri")
}

fn offset_to_position(text: &str, offset: usize) -> Position {
    let mut line: u32 = 0;
    let mut col_utf16: u32 = 0;
    let mut cur: usize = 0;

    for ch in text.chars() {
        if cur >= offset {
            break;
        }
        cur += ch.len_utf8();
        if ch == '\n' {
            line += 1;
            col_utf16 = 0;
        } else {
            col_utf16 += ch.len_utf16() as u32;
        }
    }

    Position::new(line, col_utf16)
}

fn position_to_offset(text: &str, position: Position) -> Option<usize> {
    let mut line: u32 = 0;
    let mut col_utf16: u32 = 0;
    let mut offset: usize = 0;

    for ch in text.chars() {
        if line == position.line && col_utf16 == position.character {
            return Some(offset);
        }

        offset += ch.len_utf8();
        if ch == '\n' {
            line += 1;
            col_utf16 = 0;
        } else {
            col_utf16 += ch.len_utf16() as u32;
        }
    }

    if line == position.line && col_utf16 == position.character {
        Some(offset)
    } else {
        None
    }
}

fn range_text<'a>(text: &'a str, range: Range) -> &'a str {
    let start = position_to_offset(text, range.start).unwrap();
    let end = position_to_offset(text, range.end).unwrap();
    &text[start..end]
}

fn diagnostic_messages(resp: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(items) = resp.pointer("/result/items").and_then(|v| v.as_array()) else {
        return out;
    };
    for item in items {
        if let Some(msg) = item.get("message").and_then(|m| m.as_str()) {
            out.push(msg.to_string());
        }
    }
    out
}

#[test]
fn did_change_watched_files_updates_cached_analysis_state() {
    let _lock = crate::support::stdio_server_lock();
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
        "expected cached missing state to stay in effect until didChangeWatchedFiles"
    );

    // 3) Notify about file creation; diagnostics should now see the unresolved reference.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "workspace/didChangeWatchedFiles",
            "params": {
                "changes": [{ "uri": uri, "type": 1 }]
            }
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
    assert!(diagnostic_messages(&resp)
        .iter()
        .any(|m| m.contains("Cannot resolve symbol 'bar'")));

    // 4) Fix the file on disk but don't notify; diagnostics should stay stale.
    let fixed = r#"class Main {
    void bar() {}
    void test() {
        bar();
    }
}
"#;
    std::fs::write(&file_path, fixed).expect("rewrite Main.java");

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
        "expected cached contents to remain until didChangeWatchedFiles"
    );

    // 5) Notify about the change; diagnostics should clear.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "workspace/didChangeWatchedFiles",
            "params": {
                "changes": [{ "uri": uri, "type": 2 }]
            }
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
    let messages = diagnostic_messages(&resp);
    assert!(
        messages
            .iter()
            .all(|m| !m.contains("Cannot resolve symbol 'bar'")),
        "expected didChangeWatchedFiles to refresh cached contents, but still saw unresolved `bar`: {messages:?}"
    );

    // Confirm that go-to-definition sees the updated on-disk file.
    let offset = fixed.find("bar();").unwrap() + 1;
    let position = offset_to_position(fixed, offset);

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "textDocument/definition",
            "params": { "textDocument": { "uri": uri }, "position": position }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 7);
    let location = resp.get("result").cloned().expect("definition result");
    let range: Range =
        serde_json::from_value(location.get("range").cloned().expect("range")).expect("range");
    assert_eq!(range_text(fixed, range), "bar");

    // 6) Delete on disk without notifying; definition should still use cached content.
    std::fs::remove_file(&file_path).expect("remove Main.java");
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 8,
            "method": "textDocument/definition",
            "params": { "textDocument": { "uri": uri }, "position": position }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 8);
    assert!(resp.get("result").is_some_and(|v| !v.is_null()));

    // 7) Notify about deletion; definition should now treat the file as missing.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "workspace/didChangeWatchedFiles",
            "params": {
                "changes": [{ "uri": uri, "type": 3 }]
            }
        }),
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 9,
            "method": "textDocument/definition",
            "params": { "textDocument": { "uri": uri }, "position": position }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 9);
    assert!(resp.get("result").is_some_and(|v| v.is_null()));

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 10, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 10);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn did_change_watched_files_reloads_nova_config() {
    let _lock = crate::support::stdio_server_lock();
    let temp = TempDir::new().expect("tempdir");
    let config_path = temp.path().join("nova.toml");
    let config_uri = uri_for_path(&config_path);

    std::fs::write(&config_path, "[extensions]\nenabled = false\n").expect("write nova.toml");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        // Ensure a developer's legacy AI env-var wiring can't override the config file and make
        // this test flaky.
        .env_remove("NOVA_AI_PROVIDER")
        .env_remove("NOVA_AI_ENDPOINT")
        .env_remove("NOVA_AI_MODEL")
        .env_remove("NOVA_AI_API_KEY")
        .env_remove("NOVA_AI_AUDIT_LOGGING")
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
            "id": 2,
            "method": "nova/extensions/status",
            "params": null
        }),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    assert_eq!(
        resp.pointer("/result/enabled").and_then(|v| v.as_bool()),
        Some(false),
        "expected initial config to disable extensions"
    );

    // Update config on disk but don't notify; the server should keep the cached config.
    std::fs::write(&config_path, "[extensions]\nenabled = true\n").expect("rewrite nova.toml");

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "nova/extensions/status",
            "params": null
        }),
    );
    let resp = read_response_with_id(&mut stdout, 3);
    assert_eq!(
        resp.pointer("/result/enabled").and_then(|v| v.as_bool()),
        Some(false),
        "expected cached config to remain in effect until didChangeWatchedFiles"
    );

    // Notify about the file change; the server should reload `nova_config` from disk.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "workspace/didChangeWatchedFiles",
            "params": {
                "changes": [{ "uri": config_uri, "type": 2 }]
            }
        }),
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "nova/extensions/status",
            "params": null
        }),
    );
    let resp = read_response_with_id(&mut stdout, 4);
    assert_eq!(
        resp.pointer("/result/enabled").and_then(|v| v.as_bool()),
        Some(true),
        "expected didChangeWatchedFiles to reload nova.toml"
    );

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

#[test]
fn did_change_watched_files_falls_back_to_default_config_when_config_is_deleted() {
    let _lock = crate::support::stdio_server_lock();
    let temp = TempDir::new().expect("tempdir");
    let config_path = temp.path().join("nova.toml");
    let config_uri = uri_for_path(&config_path);

    std::fs::write(&config_path, "[extensions]\nenabled = false\n").expect("write nova.toml");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
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
            "id": 2,
            "method": "nova/extensions/status",
            "params": null
        }),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    assert_eq!(
        resp.pointer("/result/enabled").and_then(|v| v.as_bool()),
        Some(false),
        "expected initial config to disable extensions"
    );

    // Delete the config on disk but don't notify; the server should keep the cached config.
    std::fs::remove_file(&config_path).expect("remove nova.toml");
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "nova/extensions/status",
            "params": null
        }),
    );
    let resp = read_response_with_id(&mut stdout, 3);
    assert_eq!(
        resp.pointer("/result/enabled").and_then(|v| v.as_bool()),
        Some(false),
        "expected cached config to remain in effect until didChangeWatchedFiles"
    );

    // Notify about the deletion; the server should fall back to defaults.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "workspace/didChangeWatchedFiles",
            "params": {
                "changes": [{ "uri": config_uri, "type": 3 }]
            }
        }),
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "nova/extensions/status",
            "params": null
        }),
    );
    let resp = read_response_with_id(&mut stdout, 4);
    assert_eq!(
        resp.pointer("/result/enabled").and_then(|v| v.as_bool()),
        Some(true),
        "expected didChangeWatchedFiles to fall back to defaults when nova.toml is deleted"
    );

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
