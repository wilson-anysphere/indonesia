use nova_core::{path_to_file_uri, AbsPathBuf, LineIndex, TextSize};
use std::io::BufReader;
use std::path::Path;
use std::process::{Command, Stdio};
use tempfile::TempDir;

use lsp_types::{
    GotoDefinitionParams, GotoDefinitionResponse, Location, PartialResultParams, Position,
    TextDocumentIdentifier, TextDocumentPositionParams, Uri, WorkDoneProgressParams,
};

use crate::support::{
    did_open_notification, exit_notification, initialize_request_empty, initialized_notification,
    jsonrpc_request, read_response_with_id, shutdown_request, stdio_server_lock,
    write_jsonrpc_message,
};

fn uri_for_path(path: &Path) -> Uri {
    let abs = AbsPathBuf::try_from(path.to_path_buf()).expect("abs path");
    path_to_file_uri(&abs)
        .expect("file uri")
        .parse()
        .expect("uri")
}

fn utf16_position(text: &str, offset: usize) -> nova_core::Position {
    let index = LineIndex::new(text);
    let offset = TextSize::from(u32::try_from(offset).expect("offset fits in u32"));
    index.position(text, offset)
}

fn goto_params(uri: Uri, pos: nova_core::Position) -> GotoDefinitionParams {
    GotoDefinitionParams {
        text_document_position_params: TextDocumentPositionParams::new(
            TextDocumentIdentifier { uri },
            Position::new(pos.line, pos.character),
        ),
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    }
}

fn decode_goto_locations(value: serde_json::Value) -> Vec<Location> {
    let Some(response) =
        serde_json::from_value::<Option<GotoDefinitionResponse>>(value).expect("goto response")
    else {
        return Vec::new();
    };

    match response {
        GotoDefinitionResponse::Scalar(location) => vec![location],
        GotoDefinitionResponse::Array(locations) => locations,
        GotoDefinitionResponse::Link(links) => panic!("unexpected location links: {links:?}"),
    }
}

#[test]
fn stdio_server_handles_implementation_declaration_and_type_definition_requests() {
    let _lock = stdio_server_lock();
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();

    let iface_path = root.join("I.java");
    let impl_path = root.join("C.java");
    let foo_path = root.join("Foo.java");
    let main_path = root.join("Main.java");

    let iface_uri = uri_for_path(&iface_path);
    let impl_uri = uri_for_path(&impl_path);
    let foo_uri = uri_for_path(&foo_path);
    let main_uri = uri_for_path(&main_path);

    let iface_text = "interface I {\n    void foo();\n}\n";
    let impl_text = "class C implements I {\n    public void foo() {}\n}\n";
    let foo_text = "class Foo {}\n";
    let main_text = concat!(
        "class Main {\n",
        "    void test() {\n",
        "        Foo foo = new Foo();\n",
        "        foo.toString();\n",
        "    }\n",
        "}\n",
    );

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
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

    for (uri, text) in [
        (&iface_uri, iface_text),
        (&impl_uri, impl_text),
        (&foo_uri, foo_text),
        (&main_uri, main_text),
    ] {
        write_jsonrpc_message(
            &mut stdin,
            &did_open_notification(uri.clone(), "java", 1, text),
        );
    }

    // 1) implementation: interface method -> implementing method.
    let iface_foo_offset = iface_text.find("foo").expect("foo in interface");
    let iface_foo_pos = utf16_position(iface_text, iface_foo_offset);
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            goto_params(iface_uri.clone(), iface_foo_pos),
            2,
            "textDocument/implementation",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let locations = decode_goto_locations(resp.get("result").cloned().unwrap_or_default());
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].uri, impl_uri);

    let impl_foo_offset = impl_text.find("foo").expect("foo in impl");
    let impl_foo_pos = utf16_position(impl_text, impl_foo_offset);
    assert_eq!(
        locations[0].range.start,
        Position::new(impl_foo_pos.line, impl_foo_pos.character)
    );

    // 2) declaration: override -> interface declaration.
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            goto_params(impl_uri.clone(), impl_foo_pos),
            3,
            "textDocument/declaration",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 3);
    let mut locations = decode_goto_locations(resp.get("result").cloned().unwrap_or_default());
    assert_eq!(locations.len(), 1);
    let loc = locations.pop().expect("location");
    assert_eq!(loc.uri, iface_uri);
    assert_eq!(
        loc.range.start,
        Position::new(iface_foo_pos.line, iface_foo_pos.character)
    );

    // 3) typeDefinition: variable usage -> class definition.
    let usage_offset = main_text.find("foo.toString").expect("foo usage in main");
    let usage_pos = utf16_position(main_text, usage_offset);
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            goto_params(main_uri.clone(), usage_pos),
            4,
            "textDocument/typeDefinition",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 4);
    let locations = decode_goto_locations(resp.get("result").cloned().unwrap_or_default());
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].uri, foo_uri);

    // shutdown + exit
    write_jsonrpc_message(&mut stdin, &shutdown_request(5));
    let _shutdown_resp = read_response_with_id(&mut stdout, 5);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
