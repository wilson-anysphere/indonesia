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
    let _lock = crate::support::stdio_server_lock();
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

    let cache_dir = TempDir::new().expect("cache dir");

    let main_path = root.join("Main.java");
    let main_uri: Uri = uri_for_path(&main_path).parse().expect("main uri");
    let text = "class Main { void m() { String s = \"\"; } }";
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
            "textDocument/definition",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let location = resp.get("result").expect("definition result");
    let Some(uri) = location.get("uri").and_then(|v| v.as_str()) else {
        panic!("expected definition uri, got: {resp:?}");
    };
    let uri = uri.to_string();

    assert!(
        uri.starts_with("nova:///decompiled/"),
        "expected decompiled uri, got: {uri}"
    );
    assert!(
        nova_decompile::parse_decompiled_uri(&uri).is_some(),
        "expected decompiled uri to be canonical/parseable, got: {uri}"
    );

    // Ask the server to parse the decompiled buffer via documentSymbol. This exercises that the
    // VFS can load the virtual document returned from definition.
    let uri: Uri = uri.parse().expect("decompiled uri");
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            DocumentSymbolParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
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
        "expected decompiled document to return symbols, got: {resp:?}"
    );

    write_jsonrpc_message(&mut stdin, &shutdown_request(4));
    let _shutdown_resp = read_response_with_id(&mut stdout, 4);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());

    // Spawn a second server pointing at the same cache directory to prove the decompiled document
    // persisted, and can be loaded without re-running definition.
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
                text_document: TextDocumentIdentifier { uri: uri.clone() },
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

    write_jsonrpc_message(&mut stdin, &shutdown_request(3));
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
