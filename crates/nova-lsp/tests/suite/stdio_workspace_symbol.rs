use lsp_types::{
    CancelParams, Location, NumberOrString, OneOf, PartialResultParams, SymbolInformation, Uri,
    WorkDoneProgressParams, WorkspaceLocation, WorkspaceSymbol, WorkspaceSymbolParams,
    WorkspaceSymbolResponse,
};
use std::io::BufReader;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tempfile::TempDir;

use crate::support::{
    decode_initialize_result, exit_notification, file_uri, initialize_request_with_root_uri,
    initialized_notification, jsonrpc_notification, jsonrpc_request, read_response_with_id,
    shutdown_request, write_jsonrpc_message,
};

fn symbol_uri(sym: &SymbolInformation) -> &Uri {
    &sym.location.uri
}

fn workspace_symbol_uri(sym: &WorkspaceSymbol) -> &Uri {
    match &sym.location {
        OneOf::Left(Location { uri, .. }) => uri,
        OneOf::Right(WorkspaceLocation { uri }) => uri,
    }
}

fn has_symbol_named_at_uri(
    result: WorkspaceSymbolResponse,
    name: &str,
    expected_uri: &Uri,
) -> bool {
    match result {
        WorkspaceSymbolResponse::Flat(items) => items
            .into_iter()
            .any(|sym| sym.name == name && symbol_uri(&sym) == expected_uri),
        WorkspaceSymbolResponse::Nested(items) => items
            .into_iter()
            .any(|sym| sym.name == name && workspace_symbol_uri(&sym) == expected_uri),
    }
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

    let root_uri = file_uri(root);
    let file_uri = file_uri(&file_path);

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
        &initialize_request_with_root_uri(1, root_uri.as_str().to_string()),
    );
    let initialize_resp = read_response_with_id(&mut stdout, 1);
    let init = decode_initialize_result(&initialize_resp);
    assert!(
        init.capabilities.workspace_symbol_provider.is_some(),
        "expected workspaceSymbolProvider capability: {initialize_resp:#}"
    );

    write_jsonrpc_message(&mut stdin, &initialized_notification());

    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            WorkspaceSymbolParams {
                partial_result_params: PartialResultParams::default(),
                work_done_progress_params: WorkDoneProgressParams::default(),
                query: "".to_string(),
            },
            2,
            "workspace/symbol",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let results: WorkspaceSymbolResponse =
        serde_json::from_value(resp.get("result").cloned().expect("result"))
            .expect("decode workspace/symbol response");

    assert!(
        has_symbol_named_at_uri(results, "Foo", &file_uri),
        "expected to find Foo symbol pointing at Foo.java when query is empty"
    );

    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            WorkspaceSymbolParams {
                partial_result_params: PartialResultParams::default(),
                work_done_progress_params: WorkDoneProgressParams::default(),
                query: "Foo".to_string(),
            },
            3,
            "workspace/symbol",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 3);
    let results: WorkspaceSymbolResponse =
        serde_json::from_value(resp.get("result").cloned().expect("result"))
            .expect("decode workspace/symbol response");

    assert!(
        has_symbol_named_at_uri(results, "Foo", &file_uri),
        "expected to find Foo symbol pointing at Foo.java when query is Foo"
    );

    write_jsonrpc_message(&mut stdin, &shutdown_request(4));
    let _shutdown_resp = read_response_with_id(&mut stdout, 4);
    write_jsonrpc_message(&mut stdin, &exit_notification());
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

    let root_uri = file_uri(root);
    let file_uri = file_uri(&file_path);

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
        &initialize_request_with_root_uri(1, root_uri.as_str().to_string()),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &initialized_notification());

    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            WorkspaceSymbolParams {
                partial_result_params: PartialResultParams::default(),
                work_done_progress_params: WorkDoneProgressParams::default(),
                query: "Foo".to_string(),
            },
            2,
            "workspace/symbol",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let results: WorkspaceSymbolResponse =
        serde_json::from_value(resp.get("result").cloned().expect("result"))
            .expect("decode workspace/symbol response");

    let (line, character) = match results {
        WorkspaceSymbolResponse::Flat(items) => {
            let sym = items
                .into_iter()
                .find(|sym| sym.name == "Foo" && sym.location.uri == file_uri)
                .unwrap_or_else(|| {
                    panic!("expected Foo symbol pointing at Foo.java, got: {resp:?}")
                });
            (
                sym.location.range.start.line,
                sym.location.range.start.character,
            )
        }
        WorkspaceSymbolResponse::Nested(items) => {
            let sym = items
                .into_iter()
                .find(|sym| sym.name == "Foo" && workspace_symbol_uri(sym) == &file_uri)
                .unwrap_or_else(|| {
                    panic!("expected Foo symbol pointing at Foo.java, got: {resp:?}")
                });
            match sym.location {
                OneOf::Left(loc) => (loc.range.start.line, loc.range.start.character),
                OneOf::Right(_) => {
                    panic!("expected WorkspaceSymbol location to include a range: {resp:?}")
                }
            }
        }
    };

    assert_eq!(line, expected.line);
    assert_eq!(character, expected.character);

    write_jsonrpc_message(&mut stdin, &shutdown_request(3));
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &exit_notification());
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
    let root_uri = file_uri(&root);
    let file_uri = file_uri(&file_path);

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
        &initialize_request_with_root_uri(1, root_uri.as_str().to_string()),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &initialized_notification());

    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            WorkspaceSymbolParams {
                partial_result_params: PartialResultParams::default(),
                work_done_progress_params: WorkDoneProgressParams::default(),
                query: "Foo".to_string(),
            },
            2,
            "workspace/symbol",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    assert!(
        has_symbol_named_at_uri(
            serde_json::from_value(resp.get("result").cloned().expect("result"))
                .expect("decode workspace/symbol response"),
            "Foo",
            &file_uri,
        ),
        "expected Foo symbol in percent-encoded workspace root, got: {resp:?}"
    );

    write_jsonrpc_message(&mut stdin, &shutdown_request(3));
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &exit_notification());
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

    let root_uri = file_uri(root);

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
            &initialize_request_with_root_uri(1, root_uri.as_str().to_string()),
        );
    }
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    {
        let mut stdin = stdin.lock().expect("lock stdin");
        write_jsonrpc_message(&mut *stdin, &initialized_notification());
    }

    {
        let mut stdin = stdin.lock().expect("lock stdin");
        write_jsonrpc_message(
            &mut *stdin,
            &jsonrpc_request(
                WorkspaceSymbolParams {
                    partial_result_params: PartialResultParams::default(),
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    query: "Foo".to_string(),
                },
                2,
                "workspace/symbol",
            ),
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
                    &jsonrpc_notification(
                        CancelParams {
                            id: NumberOrString::Number(2),
                        },
                        "$/cancelRequest",
                    ),
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
        write_jsonrpc_message(&mut *stdin, &shutdown_request(3));
    }
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    {
        let mut stdin = stdin.lock().expect("lock stdin");
        write_jsonrpc_message(&mut *stdin, &exit_notification());
    }
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
