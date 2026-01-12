use nova_core::{path_to_file_uri, AbsPathBuf};
use serde_json::json;
use std::io::BufReader;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tempfile::TempDir;

use crate::support::{read_response_with_id, write_jsonrpc_message};

fn uri_for_path(path: &Path) -> String {
    let abs = AbsPathBuf::canonicalize(path).expect("abs path");
    path_to_file_uri(&abs).expect("file uri")
}

#[test]
fn stdio_server_supports_workspace_symbol_requests() {
    let _lock = crate::support::stdio_server_lock();
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();

    let cache_dir = TempDir::new().expect("cache dir");

    let file_path = root.join("Foo.java");
    std::fs::write(
        &file_path,
        r#"
            package com.example;

            public class Foo {
                public void bar() {}
            }
        "#,
    )
    .expect("write java file");

    let root_uri = uri_for_path(root);
    let file_uri = uri_for_path(&file_path);

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
    assert!(
        initialize_resp
            .pointer("/result/capabilities/workspaceSymbolProvider")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        "expected workspaceSymbolProvider capability"
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "workspace/symbol",
            "params": { "query": "" }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let results = resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("workspace/symbol result array");

    assert!(
        results.iter().any(|value| {
            value.get("name").and_then(|v| v.as_str()) == Some("Foo")
                && value.pointer("/location/uri").and_then(|v| v.as_str())
                    == Some(file_uri.as_str())
        }),
        "expected to find Foo symbol pointing at Foo.java when query is empty"
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "workspace/symbol",
            "params": { "query": "Foo" }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 3);
    let results = resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("workspace/symbol result array");

    assert!(
        results.iter().any(|value| {
            value.get("name").and_then(|v| v.as_str()) == Some("Foo")
                && value.pointer("/location/uri").and_then(|v| v.as_str())
                    == Some(file_uri.as_str())
        }),
        "expected to find Foo symbol pointing at Foo.java when query is Foo"
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

#[test]
fn stdio_workspace_symbol_reports_utf16_definition_positions() {
    let _lock = crate::support::stdio_server_lock();
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    let cache_dir = TempDir::new().expect("cache dir");

    let file_path = root.join("Foo.java");
    let file_text =
        "package com.example;\n\n/* 🦀 */ public class Foo {\n    public void bar() {}\n}\n";
    std::fs::write(&file_path, file_text).expect("write java file");

    let name_offset = file_text.find("Foo").expect("class name");
    let line_index = nova_core::LineIndex::new(file_text);
    let expected = line_index.position(file_text, nova_core::TextSize::from(name_offset as u32));
    assert_eq!(expected.line, 2, "expected Foo on line 2 (0-based)");
    assert_eq!(
        expected.character, 22,
        "expected UTF-16 column to count the emoji as a surrogate pair"
    );

    let root_uri = uri_for_path(root);
    let file_uri = uri_for_path(&file_path);

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
            "method": "workspace/symbol",
            "params": { "query": "Foo" }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let results = resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("workspace/symbol result array");

    let foo = results
        .iter()
        .find(|value| {
            value.get("name").and_then(|v| v.as_str()) == Some("Foo")
                && value.pointer("/location/uri").and_then(|v| v.as_str())
                    == Some(file_uri.as_str())
        })
        .unwrap_or_else(|| panic!("expected Foo symbol pointing at Foo.java, got: {resp:?}"));

    let line = foo
        .pointer("/location/range/start/line")
        .and_then(|v| v.as_u64())
        .expect("location.range.start.line");
    let character = foo
        .pointer("/location/range/start/character")
        .and_then(|v| v.as_u64())
        .expect("location.range.start.character");

    assert_eq!(line as u32, expected.line);
    assert_eq!(character as u32, expected.character);

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
fn stdio_workspace_symbol_supports_root_uri_with_percent_encoding() {
    let _lock = crate::support::stdio_server_lock();
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path().join("My Project");
    std::fs::create_dir_all(&root).expect("create workspace root");

    let cache_dir = TempDir::new().expect("cache dir");

    let file_path = root.join("Foo.java");
    std::fs::write(
        &file_path,
        r#"
            package com.example;

            public class Foo {
                public void bar() {}
            }
        "#,
    )
    .expect("write java file");

    // `path_to_file_uri` percent-encodes spaces. This ensures the server decodes
    // the initialize.rootUri back into a usable on-disk path.
    let root_uri = uri_for_path(&root);
    let file_uri = uri_for_path(&file_path);

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
            "method": "workspace/symbol",
            "params": { "query": "Foo" }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let results = resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("workspace/symbol result array");
    assert!(
        results.iter().any(|value| {
            value.get("name").and_then(|v| v.as_str()) == Some("Foo")
                && value.pointer("/location/uri").and_then(|v| v.as_str())
                    == Some(file_uri.as_str())
        }),
        "expected Foo symbol in percent-encoded workspace root, got: {resp:?}"
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
fn stdio_cancel_request_interrupts_workspace_symbol_indexing() {
    let _lock = crate::support::stdio_server_lock();
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();

    let cache_dir = TempDir::new().expect("cache dir");

    // Create enough files to ensure `workspace/symbol` spends time indexing so that cancellation
    // happens while the request is in flight (not just before the handler starts).
    for i in 0..500 {
        let file_path = root.join(format!("Foo{i}.java"));
        std::fs::write(
            &file_path,
            format!(
                r#"
                    package com.example;

                    public class Foo{i} {{
                        public void bar{i}() {{}}
                    }}
                "#
            ),
        )
        .expect("write java file");
    }

    let root_uri = uri_for_path(root);

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .env("NOVA_CACHE_DIR", cache_dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let stdin = child.stdin.take().expect("stdin");
    let stdin = Arc::new(Mutex::new(stdin));
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    {
        let mut stdin = stdin.lock().expect("lock stdin");
        write_jsonrpc_message(
            &mut *stdin,
            &json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": { "rootUri": root_uri, "capabilities": {} }
            }),
        );
    }
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    {
        let mut stdin = stdin.lock().expect("lock stdin");
        write_jsonrpc_message(
            &mut *stdin,
            &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
        );
    }

    {
        let mut stdin = stdin.lock().expect("lock stdin");
        write_jsonrpc_message(
            &mut *stdin,
            &json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "workspace/symbol",
                "params": { "query": "Foo" }
            }),
        );
    }

    // Cancel after a short delay to give the request a chance to enter the indexing loop.
    let cancel_stdin = stdin.clone();
    let cancel_done = Arc::new(AtomicBool::new(false));
    let cancel_done_thread = cancel_done.clone();
    let cancel_thread = std::thread::spawn(move || {
        // Cancellation can race with request registration inside the server/router thread.
        // Keep retrying for a short window so we reliably cancel the in-flight request.
        for _ in 0..200 {
            if cancel_done_thread.load(Ordering::SeqCst) {
                break;
            }
            {
                let mut stdin = cancel_stdin.lock().expect("lock stdin");
                write_jsonrpc_message(
                    &mut *stdin,
                    &json!({ "jsonrpc": "2.0", "method": "$/cancelRequest", "params": { "id": 2 } }),
                );
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    });

    let resp = read_response_with_id(&mut stdout, 2);
    cancel_done.store(true, Ordering::SeqCst);
    let code = resp
        .get("error")
        .and_then(|err| err.get("code"))
        .and_then(|v| v.as_i64());
    assert_eq!(
        code,
        Some(-32800),
        "expected cancelled workspace/symbol request to return -32800, got: {resp:?}"
    );

    cancel_thread.join().expect("cancel thread");

    {
        let mut stdin = stdin.lock().expect("lock stdin");
        write_jsonrpc_message(
            &mut *stdin,
            &json!({ "jsonrpc": "2.0", "id": 3, "method": "shutdown" }),
        );
    }
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    {
        let mut stdin = stdin.lock().expect("lock stdin");
        write_jsonrpc_message(&mut *stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    }
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
