#![cfg(unix)]

use nova_core::{path_to_file_uri, AbsPathBuf};
use serde_json::json;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

use crate::support::{read_response_with_id, stdio_server_lock, write_jsonrpc_message};

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
        .env("NOVA_CARGO_JOBS", "1")
        .arg("-p")
        .arg("nova-worker")
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
        "expected to find Foo symbol pointing at Foo.java in distributed mode, got: {resp:?}"
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
