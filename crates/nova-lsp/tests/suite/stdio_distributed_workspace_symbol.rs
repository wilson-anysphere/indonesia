#![cfg(unix)]

use lsp_types::{FileChangeType, FileEvent, Uri, WorkspaceSymbolParams};
use nova_core::{path_to_file_uri, AbsPathBuf};
use serde_json::Value;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use tempfile::TempDir;

use crate::support::{
    exit_notification, initialize_request_with_root_uri, initialized_notification,
    jsonrpc_notification, jsonrpc_request, read_response_with_id, shutdown_request,
    stdio_server_lock, write_jsonrpc_message,
};

fn uri_for_path(path: &Path) -> String {
    let abs = AbsPathBuf::canonicalize(path).expect("abs path");
    path_to_file_uri(&abs).expect("file uri")
}

fn ensure_worker_binary() -> PathBuf {
    let lsp_bin = PathBuf::from(env!("CARGO_BIN_EXE_nova-lsp"));
    let bin_dir = lsp_bin.parent().expect("nova-lsp bin dir");
    let worker_bin = bin_dir.join("nova-worker");
    if worker_bin.is_file() {
        return worker_bin;
    }

    // `cargo test --locked -p nova-lsp --tests` does not build the `nova-worker` binary by default.
    // Build it on demand so distributed mode can spawn local workers.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir.join("../..");
    let status = Command::new("bash")
        .arg(repo_root.join("scripts/cargo_agent.sh"))
        .arg("build")
        .arg("--quiet")
        // Keep this build serial to avoid exhausting thread/process limits in constrained test
        // environments. Also match the `test` profile settings (notably, reduced debuginfo) to
        // stay within the default `scripts/cargo_agent.sh` RLIMIT_AS budget.
        //
        // Use the wrapper's env var rather than passing `-j` directly so this stays compatible
        // even when callers set `NOVA_CARGO_JOBS` globally (which would otherwise result in
        // duplicate `--jobs` flags).
        .arg("--profile")
        .arg("test")
        .arg("-p")
        .arg("nova-worker")
        .env("NOVA_CARGO_JOBS", "1")
        .current_dir(&repo_root)
        .status()
        .expect("spawn scripts/cargo_agent.sh build -p nova-worker");
    assert!(
        status.success(),
        "scripts/cargo_agent.sh build -p nova-worker failed"
    );

    // Best-effort: check common locations.
    if worker_bin.is_file() {
        return worker_bin;
    }
    let workspace_worker_bin = manifest_dir.join("../../target/debug/nova-worker");
    if workspace_worker_bin.is_file() {
        return workspace_worker_bin;
    }

    panic!(
        "expected nova-worker binary at {} or {}",
        worker_bin.display(),
        workspace_worker_bin.display()
    );
}

fn send_workspace_symbol_request(
    stdin: &mut impl std::io::Write,
    stdout: &mut impl std::io::BufRead,
    id: i64,
    query: &str,
) -> Value {
    write_jsonrpc_message(
        stdin,
        &jsonrpc_request(
            WorkspaceSymbolParams {
                query: query.to_string(),
                ..WorkspaceSymbolParams::default()
            },
            id,
            "workspace/symbol",
        ),
    );
    read_response_with_id(stdout, id)
}

fn response_contains_symbol(resp: &Value, name: &str, uri: &str) -> bool {
    resp.get("result")
        .and_then(|v| v.as_array())
        .iter()
        .flat_map(|arr| arr.iter())
        .any(|value| {
            value.get("name").and_then(|v| v.as_str()) == Some(name)
                && value.pointer("/location/uri").and_then(|v| v.as_str()) == Some(uri)
        })
}

fn wait_for_symbol(
    stdin: &mut impl std::io::Write,
    stdout: &mut impl std::io::BufRead,
    next_id: &mut i64,
    query: &str,
    name: &str,
    uri: &str,
) {
    for _ in 0..40 {
        let resp = send_workspace_symbol_request(stdin, stdout, *next_id, query);
        *next_id += 1;
        if response_contains_symbol(&resp, name, uri) {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    let resp = send_workspace_symbol_request(stdin, stdout, *next_id, query);
    panic!("timed out waiting for symbol {name} in response: {resp:?}");
}

fn wait_for_symbol_absent(
    stdin: &mut impl std::io::Write,
    stdout: &mut impl std::io::BufRead,
    next_id: &mut i64,
    query: &str,
    name: &str,
    uri: &str,
) {
    for _ in 0..40 {
        let resp = send_workspace_symbol_request(stdin, stdout, *next_id, query);
        *next_id += 1;
        if !response_contains_symbol(&resp, name, uri) {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    let resp = send_workspace_symbol_request(stdin, stdout, *next_id, query);
    panic!("timed out waiting for symbol {name} to disappear; still present: {resp:?}");
}

#[test]
fn stdio_server_supports_workspace_symbol_requests_via_distributed_router() {
    let _lock = stdio_server_lock();

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

    let worker_bin = ensure_worker_binary();

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--distributed")
        .arg("--distributed-worker-command")
        .arg(worker_bin)
        .env("NOVA_CACHE_DIR", cache_dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    write_jsonrpc_message(&mut stdin, &initialize_request_with_root_uri(1, root_uri));
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &initialized_notification());

    let resp = send_workspace_symbol_request(&mut stdin, &mut stdout, 2, "Foo");
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
        "expected to find Foo symbol pointing at Foo.java in distributed mode, got: {resp:?}"
    );

    write_jsonrpc_message(&mut stdin, &shutdown_request(3));
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_distributed_workspace_symbol_reports_utf16_definition_positions() {
    let _lock = stdio_server_lock();

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

    let worker_bin = ensure_worker_binary();

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--distributed")
        .arg("--distributed-worker-command")
        .arg(worker_bin)
        .env("NOVA_CACHE_DIR", cache_dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    write_jsonrpc_message(&mut stdin, &initialize_request_with_root_uri(1, root_uri));
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &initialized_notification());

    let mut next_id = 2i64;
    wait_for_symbol(
        &mut stdin,
        &mut stdout,
        &mut next_id,
        "Foo",
        "Foo",
        &file_uri,
    );

    let resp = send_workspace_symbol_request(&mut stdin, &mut stdout, next_id, "Foo");
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

    write_jsonrpc_message(&mut stdin, &shutdown_request(next_id + 1));
    let _shutdown_resp = read_response_with_id(&mut stdout, next_id + 1);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_distributed_router_refreshes_on_did_change_watched_files() {
    let _lock = stdio_server_lock();

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

    let worker_bin = ensure_worker_binary();

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--distributed")
        .arg("--distributed-worker-command")
        .arg(worker_bin)
        .env("NOVA_CACHE_DIR", cache_dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    write_jsonrpc_message(&mut stdin, &initialize_request_with_root_uri(1, root_uri));
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &initialized_notification());

    let mut next_id = 2i64;
    wait_for_symbol(
        &mut stdin,
        &mut stdout,
        &mut next_id,
        "Foo",
        "Foo",
        &file_uri,
    );

    std::fs::write(
        &file_path,
        r#"
            package com.example;

            public class Foo {
                public void bar() {}
            }

            class Baz {}
        "#,
    )
    .expect("rewrite java file");

    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_notification(
            lsp_types::DidChangeWatchedFilesParams {
                changes: vec![FileEvent::new(
                    file_uri
                        .parse::<Uri>()
                        .expect("file uri must be a valid LSP Uri"),
                    FileChangeType::CHANGED,
                )],
            },
            "workspace/didChangeWatchedFiles",
        ),
    );
    wait_for_symbol(
        &mut stdin,
        &mut stdout,
        &mut next_id,
        "Baz",
        "Baz",
        &file_uri,
    );

    std::fs::remove_file(&file_path).expect("delete java file");
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_notification(
            lsp_types::DidChangeWatchedFilesParams {
                changes: vec![FileEvent::new(
                    file_uri
                        .parse::<Uri>()
                        .expect("file uri must be a valid LSP Uri"),
                    FileChangeType::DELETED,
                )],
            },
            "workspace/didChangeWatchedFiles",
        ),
    );

    wait_for_symbol_absent(
        &mut stdin,
        &mut stdout,
        &mut next_id,
        "Foo",
        "Foo",
        &file_uri,
    );
    wait_for_symbol_absent(
        &mut stdin,
        &mut stdout,
        &mut next_id,
        "Baz",
        "Baz",
        &file_uri,
    );

    write_jsonrpc_message(&mut stdin, &shutdown_request(next_id));
    let _shutdown_resp = read_response_with_id(&mut stdout, next_id);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
