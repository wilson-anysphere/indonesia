use lsp_types::{Position, TextDocumentIdentifier, TextDocumentPositionParams, Uri};
use nova_core::{path_to_file_uri, AbsPathBuf, LineIndex, TextSize};
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

use crate::support::{
    did_open_notification, exit_notification, initialize_request_empty, initialized_notification,
    jsonrpc_request, read_response_with_id, shutdown_request, stdio_server_lock,
    write_jsonrpc_message,
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
fn stdio_definition_into_jdk_supports_fully_qualified_type_names() {
    let _lock = stdio_server_lock();

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
    let main_uri: Uri = uri_for_path(&main_path).parse().expect("main uri");
    let text = "class Main { java.util.List l; }";
    std::fs::write(&main_path, text).expect("write Main.java");

    // Add a workspace type with the same simple name as the JDK type to ensure the server does not
    // incorrectly resolve `java.util.List` to the workspace type.
    let list_path = root.join("p/List.java");
    std::fs::create_dir_all(list_path.parent().expect("parent dir")).expect("create p/");
    let list_uri: Uri = uri_for_path(&list_path).parse().expect("list uri");
    let list_text = "package p; public class List {}";
    std::fs::write(&list_path, list_text).expect("write p/List.java");

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

    write_jsonrpc_message(&mut stdin, &initialize_request_empty(1));
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &initialized_notification());

    write_jsonrpc_message(
        &mut stdin,
        &did_open_notification(list_uri.clone(), "java", 1, list_text),
    );

    write_jsonrpc_message(
        &mut stdin,
        &did_open_notification(main_uri.clone(), "java", 1, text),
    );

    let offset = text.find("List").expect("List token exists");
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

    assert!(
        uri.starts_with("nova:///decompiled/"),
        "expected decompiled uri, got: {uri}"
    );
    assert!(
        uri.ends_with("/java.util.List.java"),
        "expected decompiled List uri, got: {uri}"
    );

    write_jsonrpc_message(&mut stdin, &shutdown_request(3));
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
