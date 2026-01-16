use lsp_types::{
    DocumentSymbolParams, PartialResultParams, Position, TextDocumentIdentifier,
    TextDocumentPositionParams, Uri, WorkDoneProgressParams,
};
use nova_core::{path_to_file_uri, AbsPathBuf, LineIndex, TextSize};
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

use crate::support::{
    did_open_notification, exit_notification, initialize_request_empty, initialized_notification,
    jsonrpc_request, read_response_with_id, shutdown_request, write_jsonrpc_message,
};

fn uri_for_path(path: &Path) -> Uri {
    let abs = AbsPathBuf::try_from(path.to_path_buf()).expect("abs path");
    path_to_file_uri(&abs)
        .expect("file uri")
        .parse()
        .expect("lsp uri")
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

    let cache_dir = TempDir::new().expect("cache dir");

    let main_path = root.join("Main.java");
    let main_uri = uri_for_path(&main_path);
    std::fs::write(&main_path, text).expect("write Main.java");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .env("JAVA_HOME", fake_jdk_root())
        .env("NOVA_CACHE_DIR", cache_dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    write_jsonrpc_message(&mut stdin, &initialize_request_empty(1));
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &initialized_notification());

    write_jsonrpc_message(
        &mut stdin,
        &did_open_notification(main_uri.clone(), "java", 1, text),
    );

    let position = utf16_position(text, offset);
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            TextDocumentPositionParams::new(
                TextDocumentIdentifier {
                    uri: main_uri.clone(),
                },
                Position::new(position.line, position.character),
            ),
            2,
            "textDocument/typeDefinition",
        ),
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

    write_jsonrpc_message(&mut stdin, &shutdown_request(3));
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &exit_notification());
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

#[test]
fn stdio_type_definition_decompiled_uri_is_persisted_and_readable_after_restart() {
    let _lock = crate::support::stdio_server_lock();

    let fake_jdk_root = fake_jdk_root();

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();

    let cache_dir = TempDir::new().expect("cache dir");

    let main_path = root.join("Main.java");
    let main_uri = uri_for_path(&main_path);
    let text =
        "class Main { void m(){ String s = \"\"; Custom c = new Custom(); s.toString(); c.toString(); } }\n";
    std::fs::write(&main_path, text).expect("write Main.java");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .env("JAVA_HOME", &fake_jdk_root)
        .env("NOVA_CACHE_DIR", cache_dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    write_jsonrpc_message(&mut stdin, &initialize_request_empty(1));
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &initialized_notification());

    write_jsonrpc_message(
        &mut stdin,
        &did_open_notification(main_uri.clone(), "java", 1, text),
    );

    // 1) Trigger the "cursor is on a type token" branch (String).
    let offset = text.find("String").expect("String token exists");
    let position = utf16_position(text, offset);
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            TextDocumentPositionParams::new(
                TextDocumentIdentifier {
                    uri: main_uri.clone(),
                },
                Position::new(position.line, position.character),
            ),
            2,
            "textDocument/typeDefinition",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let location = resp.get("result").expect("typeDefinition result");
    let Some(string_uri) = location.get("uri").and_then(|v| v.as_str()) else {
        panic!("expected typeDefinition uri, got: {resp:?}");
    };
    let string_uri = string_uri.to_string();
    assert!(
        string_uri.starts_with("nova:///decompiled/"),
        "expected decompiled uri, got: {string_uri}"
    );

    // 2) Trigger the "infer declared type from variable" branch (Custom).
    let offset = text.find("c.toString").expect("c.toString exists");
    let position = utf16_position(text, offset);
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            TextDocumentPositionParams::new(
                TextDocumentIdentifier {
                    uri: main_uri.clone(),
                },
                Position::new(position.line, position.character),
            ),
            3,
            "textDocument/typeDefinition",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 3);
    let location = resp.get("result").expect("typeDefinition result");
    let Some(custom_uri) = location.get("uri").and_then(|v| v.as_str()) else {
        panic!("expected typeDefinition uri, got: {resp:?}");
    };
    let custom_uri = custom_uri.to_string();
    assert!(
        custom_uri.starts_with("nova:///decompiled/"),
        "expected decompiled uri, got: {custom_uri}"
    );

    // Confirm the virtual documents are readable inside the same server session.
    let string_uri: Uri = string_uri.parse().expect("string uri");
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            DocumentSymbolParams {
                text_document: TextDocumentIdentifier {
                    uri: string_uri.clone(),
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
            4,
            "textDocument/documentSymbol",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 4);
    let symbols = resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("documentSymbol result array");
    assert!(
        !symbols.is_empty(),
        "expected decompiled document to return symbols, got: {resp:?}"
    );

    let custom_uri: Uri = custom_uri.parse().expect("custom uri");
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            DocumentSymbolParams {
                text_document: TextDocumentIdentifier {
                    uri: custom_uri.clone(),
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
            5,
            "textDocument/documentSymbol",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 5);
    let symbols = resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("documentSymbol result array");
    assert!(
        !symbols.is_empty(),
        "expected decompiled document to return symbols, got: {resp:?}"
    );

    write_jsonrpc_message(&mut stdin, &shutdown_request(6));
    let _shutdown_resp = read_response_with_id(&mut stdout, 6);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());

    // Spawn a second server pointing at the same cache directory to prove the decompiled documents
    // persisted, and can be loaded without re-running typeDefinition.
    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .env("JAVA_HOME", &fake_jdk_root)
        .env("NOVA_CACHE_DIR", cache_dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp (second instance)");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    write_jsonrpc_message(&mut stdin, &initialize_request_empty(1));
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &initialized_notification());

    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            DocumentSymbolParams {
                text_document: TextDocumentIdentifier {
                    uri: string_uri.clone(),
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
            2,
            "textDocument/documentSymbol",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let symbols = resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("documentSymbol result array");
    assert!(
        !symbols.is_empty(),
        "expected persisted decompiled document to return symbols, got: {resp:?}"
    );

    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            DocumentSymbolParams {
                text_document: TextDocumentIdentifier {
                    uri: custom_uri.clone(),
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
            3,
            "textDocument/documentSymbol",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 3);
    let symbols = resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("documentSymbol result array");
    assert!(
        !symbols.is_empty(),
        "expected persisted decompiled document to return symbols, got: {resp:?}"
    );

    write_jsonrpc_message(&mut stdin, &shutdown_request(4));
    let _shutdown_resp = read_response_with_id(&mut stdout, 4);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
