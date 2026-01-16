use nova_core::{path_to_file_uri, AbsPathBuf, LineIndex, TextSize};
use std::io::BufReader;
use std::path::Path;
use std::process::{Command, Stdio};
use tempfile::TempDir;

use lsp_types::{
    Location, PartialResultParams, Position, ReferenceContext, ReferenceParams,
    TextDocumentIdentifier, TextDocumentPositionParams, Uri, WorkDoneProgressParams,
};

use crate::support::{
    decode_initialize_result, did_open_notification, exit_notification,
    initialize_request_with_root_uri, initialized_notification, jsonrpc_request,
    read_response_with_id, shutdown_request, stdio_server_lock, write_jsonrpc_message,
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
fn stdio_server_supports_text_document_references_for_open_documents() {
    let _lock = stdio_server_lock();

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    let root_uri = uri_for_path(root);

    let cache_dir = TempDir::new().expect("cache dir");

    let foo_path = root.join("Foo.java");
    let main_path = root.join("Main.java");

    let foo_uri: Uri = uri_for_path(&foo_path).parse().expect("foo uri");
    let main_uri: Uri = uri_for_path(&main_path).parse().expect("main uri");

    let foo_text = concat!("public class Foo {\n", "    public void foo() {}\n", "}\n",);

    let main_text = concat!(
        "public class Main {\n",
        "    public void test() {\n",
        "        Foo foo = new Foo();\n",
        "        foo.foo();\n",
        "    }\n",
        "}\n",
    );

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

    write_jsonrpc_message(&mut stdin, &initialize_request_with_root_uri(1, root_uri));
    let initialize_resp = read_response_with_id(&mut stdout, 1);
    let init = decode_initialize_result(&initialize_resp);
    assert!(
        init.capabilities.references_provider.is_some(),
        "server must advertise referencesProvider: {initialize_resp:#}"
    );
    write_jsonrpc_message(&mut stdin, &initialized_notification());

    for (uri, text) in [(&foo_uri, foo_text), (&main_uri, main_text)] {
        write_jsonrpc_message(
            &mut stdin,
            &did_open_notification(uri.clone(), "java", 1, text),
        );
    }

    let foo_def_offset = foo_text.find("void foo").expect("foo decl") + "void ".len();
    let foo_def_pos = utf16_position(foo_text, foo_def_offset);
    let foo_usage_offset = main_text.find(".foo()").expect("foo usage") + ".".len();
    let foo_usage_pos = utf16_position(main_text, foo_usage_offset);

    // 1) includeDeclaration: false (should still return cross-file usage).
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            ReferenceParams {
                text_document_position: TextDocumentPositionParams::new(
                    TextDocumentIdentifier {
                        uri: foo_uri.clone(),
                    },
                    Position::new(foo_def_pos.line, foo_def_pos.character),
                ),
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
                context: ReferenceContext {
                    include_declaration: false,
                },
            },
            2,
            "textDocument/references",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let locations: Vec<Location> =
        serde_json::from_value(resp.get("result").cloned().unwrap_or_default())
            .expect("references result array");

    assert!(
        locations.iter().any(|loc| {
            loc.uri == main_uri
                && loc.range.start == Position::new(foo_usage_pos.line, foo_usage_pos.character)
        }),
        "expected references to include usage in Main.java; got {locations:?}"
    );

    assert!(
        !locations.iter().any(|loc| {
            loc.uri == foo_uri
                && loc.range.start == Position::new(foo_def_pos.line, foo_def_pos.character)
        }),
        "expected includeDeclaration=false to omit foo declaration; got {locations:?}"
    );

    // 2) includeDeclaration: true (should include declaration).
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            ReferenceParams {
                text_document_position: TextDocumentPositionParams::new(
                    TextDocumentIdentifier {
                        uri: foo_uri.clone(),
                    },
                    Position::new(foo_def_pos.line, foo_def_pos.character),
                ),
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
                context: ReferenceContext {
                    include_declaration: true,
                },
            },
            3,
            "textDocument/references",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 3);
    let locations: Vec<Location> =
        serde_json::from_value(resp.get("result").cloned().unwrap_or_default())
            .expect("references result array");
    assert!(
        locations.iter().any(|loc| {
            loc.uri == foo_uri
                && loc.range.start == Position::new(foo_def_pos.line, foo_def_pos.character)
        }),
        "expected includeDeclaration=true to include foo declaration; got {locations:?}"
    );

    // shutdown + exit
    write_jsonrpc_message(&mut stdin, &shutdown_request(4));
    let _shutdown_resp = read_response_with_id(&mut stdout, 4);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
