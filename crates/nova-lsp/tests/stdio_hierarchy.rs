use lsp_types::{
    CallHierarchyItem, CallHierarchyOutgoingCall, CallHierarchyOutgoingCallsParams,
    CallHierarchyPrepareParams, Position, TextDocumentIdentifier, TextDocumentPositionParams,
    TypeHierarchyItem, TypeHierarchyPrepareParams, TypeHierarchySupertypesParams,
};
use pretty_assertions::assert_eq;
use serde_json::json;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use tempfile::TempDir;

fn write_jsonrpc_message(writer: &mut impl Write, message: &serde_json::Value) {
    let bytes = serde_json::to_vec(message).expect("serialize");
    write!(writer, "Content-Length: {}\r\n\r\n", bytes.len()).expect("write header");
    writer.write_all(&bytes).expect("write body");
    writer.flush().expect("flush");
}

fn read_jsonrpc_message(reader: &mut impl BufRead) -> serde_json::Value {
    let mut content_length: Option<usize> = None;

    loop {
        let mut line = String::new();
        let bytes_read = reader.read_line(&mut line).expect("read header line");
        assert!(bytes_read > 0, "unexpected EOF while reading headers");

        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }

        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("Content-Length") {
                content_length = value.trim().parse::<usize>().ok();
            }
        }
    }

    let len = content_length.expect("Content-Length header");
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).expect("read body");
    serde_json::from_slice(&buf).expect("parse json")
}

fn read_jsonrpc_response_with_id(reader: &mut impl BufRead, id: i64) -> serde_json::Value {
    loop {
        let msg = read_jsonrpc_message(reader);
        if msg.get("id").and_then(|v| v.as_i64()) == Some(id) {
            return msg;
        }
    }
}

#[test]
fn stdio_call_hierarchy_outgoing_calls_includes_called_method() {
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    let file_path = root.join("Foo.java");

    let source = r#"public class Foo {
    void a() {
        b();
    }

    void b() {}
}
"#;
    fs::write(&file_path, source).expect("write Foo.java");

    let uri = format!("file://{}", file_path.to_string_lossy());
    let root_uri = format!("file://{}", root.to_string_lossy());

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
            "params": {
                "rootUri": root_uri,
                "capabilities": {}
            }
        }),
    );
    let _initialize_resp = read_jsonrpc_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": uri,
                    "languageId": "java",
                    "version": 1,
                    "text": source
                }
            }
        }),
    );

    let a_offset = source.find("void a").expect("contains void a") + "void ".len();
    let index = nova_core::LineIndex::new(source);
    let a_pos = index.position(source, nova_core::TextSize::from(a_offset as u32));
    let a_pos = Position::new(a_pos.line, a_pos.character);

    let prepare_params = CallHierarchyPrepareParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: uri.parse().expect("uri"),
            },
            position: a_pos,
        },
        work_done_progress_params: Default::default(),
    };
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/prepareCallHierarchy",
            "params": serde_json::to_value(prepare_params).expect("serialize params")
        }),
    );

    let prepare_resp = read_jsonrpc_response_with_id(&mut stdout, 2);
    let items = prepare_resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("prepareCallHierarchy result array");
    assert_eq!(items.len(), 1);
    let item: CallHierarchyItem = serde_json::from_value(items[0].clone()).expect("item");

    let outgoing_params = CallHierarchyOutgoingCallsParams {
        item,
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "callHierarchy/outgoingCalls",
            "params": serde_json::to_value(outgoing_params).expect("serialize params")
        }),
    );

    let outgoing_resp = read_jsonrpc_response_with_id(&mut stdout, 3);
    let calls = outgoing_resp
        .get("result")
        .cloned()
        .expect("outgoingCalls result");
    let calls: Vec<CallHierarchyOutgoingCall> =
        serde_json::from_value(calls).expect("outgoingCalls array");
    assert!(
        calls.iter().any(|call| call.to.name == "b"),
        "expected outgoing calls to include b(), got: {calls:?}"
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 4, "method": "shutdown" }),
    );
    let _shutdown_resp = read_jsonrpc_response_with_id(&mut stdout, 4);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_type_hierarchy_supertypes_includes_base_class() {
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    let file_path = root.join("Types.java");

    let source = r#"class Base {}
class Child extends Base {}
"#;
    fs::write(&file_path, source).expect("write Types.java");

    let uri = format!("file://{}", file_path.to_string_lossy());
    let root_uri = format!("file://{}", root.to_string_lossy());

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
            "params": {
                "rootUri": root_uri,
                "capabilities": {}
            }
        }),
    );
    let _initialize_resp = read_jsonrpc_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": uri,
                    "languageId": "java",
                    "version": 1,
                    "text": source
                }
            }
        }),
    );

    let child_offset = source
        .find("class Child")
        .expect("contains class Child")
        + "class ".len();
    let index = nova_core::LineIndex::new(source);
    let child_pos = index.position(source, nova_core::TextSize::from(child_offset as u32));
    let child_pos = Position::new(child_pos.line, child_pos.character);

    let prepare_params = TypeHierarchyPrepareParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: uri.parse().expect("uri"),
            },
            position: child_pos,
        },
        work_done_progress_params: Default::default(),
    };
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/prepareTypeHierarchy",
            "params": serde_json::to_value(prepare_params).expect("serialize params")
        }),
    );

    let prepare_resp = read_jsonrpc_response_with_id(&mut stdout, 2);
    let items = prepare_resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("prepareTypeHierarchy result array");
    assert_eq!(items.len(), 1);
    let item: TypeHierarchyItem = serde_json::from_value(items[0].clone()).expect("item");

    let supertypes_params = TypeHierarchySupertypesParams {
        item,
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "typeHierarchy/supertypes",
            "params": serde_json::to_value(supertypes_params).expect("serialize params")
        }),
    );

    let supertypes_resp = read_jsonrpc_response_with_id(&mut stdout, 3);
    let types = supertypes_resp
        .get("result")
        .cloned()
        .expect("supertypes result");
    let types: Vec<TypeHierarchyItem> = serde_json::from_value(types).expect("supertypes array");
    assert!(
        types.iter().any(|ty| ty.name == "Base"),
        "expected supertypes to include Base, got: {types:?}"
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 4, "method": "shutdown" }),
    );
    let _shutdown_resp = read_jsonrpc_response_with_id(&mut stdout, 4);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
