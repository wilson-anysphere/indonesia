use lsp_types::{
    CallHierarchyItem, CallHierarchyOutgoingCall, CallHierarchyOutgoingCallsParams,
    CallHierarchyPrepareParams, InitializeResult, PartialResultParams, Position,
    TextDocumentIdentifier, TextDocumentPositionParams, TypeHierarchyItem,
    TypeHierarchyPrepareParams, TypeHierarchySubtypesParams, TypeHierarchySupertypesParams, Uri,
    WorkDoneProgressParams,
};
use nova_core::{LineIndex, TextSize};
use std::io::BufReader;
use std::process::{Command, Stdio};
use tempfile::TempDir;

use crate::support::{
    did_open_notification, exit_notification, file_uri, initialize_request_empty,
    initialized_notification, jsonrpc_request, read_response_with_id, shutdown_request,
    stdio_server_lock, write_jsonrpc_message,
};

fn utf16_position(text: &str, offset: usize) -> nova_core::Position {
    let index = LineIndex::new(text);
    let offset = TextSize::from(u32::try_from(offset).expect("offset fits in u32"));
    index.position(text, offset)
}

fn call_hierarchy_prepare_params(uri: Uri, pos: nova_core::Position) -> CallHierarchyPrepareParams {
    CallHierarchyPrepareParams {
        text_document_position_params: TextDocumentPositionParams::new(
            TextDocumentIdentifier { uri },
            Position::new(pos.line, pos.character),
        ),
        work_done_progress_params: WorkDoneProgressParams::default(),
    }
}

fn call_hierarchy_outgoing_params(item: CallHierarchyItem) -> CallHierarchyOutgoingCallsParams {
    CallHierarchyOutgoingCallsParams {
        item,
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    }
}

fn type_hierarchy_prepare_params(uri: Uri, pos: nova_core::Position) -> TypeHierarchyPrepareParams {
    TypeHierarchyPrepareParams {
        text_document_position_params: TextDocumentPositionParams::new(
            TextDocumentIdentifier { uri },
            Position::new(pos.line, pos.character),
        ),
        work_done_progress_params: WorkDoneProgressParams::default(),
    }
}

fn type_hierarchy_supertypes_params(item: TypeHierarchyItem) -> TypeHierarchySupertypesParams {
    TypeHierarchySupertypesParams {
        item,
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    }
}

fn type_hierarchy_subtypes_params(item: TypeHierarchyItem) -> TypeHierarchySubtypesParams {
    TypeHierarchySubtypesParams {
        item,
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    }
}

#[test]
fn stdio_server_handles_call_and_type_hierarchy_requests() {
    let _lock = stdio_server_lock();

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    let file_path = root.join("Hierarchy.java");
    let uri = file_uri(&file_path);

    let text = concat!(
        "class A {\n",
        "}\n",
        "\n",
        "class B extends A {\n",
        "    void foo() {\n",
        "        bar();\n",
        "    }\n",
        "\n",
        "    void bar() {}\n",
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

    // initialize
    write_jsonrpc_message(&mut stdin, &initialize_request_empty(1));
    let initialize_resp = read_response_with_id(&mut stdout, 1);
    let init: InitializeResult = serde_json::from_value(
        initialize_resp
            .get("result")
            .cloned()
            .expect("initialize result"),
    )
    .expect("decode InitializeResult");
    assert!(
        init.capabilities.call_hierarchy_provider.is_some(),
        "expected callHierarchyProvider capability: {initialize_resp:#}"
    );
    assert!(
        init.capabilities.type_hierarchy_provider.is_some(),
        "expected typeHierarchyProvider capability: {initialize_resp:#}"
    );
    write_jsonrpc_message(&mut stdin, &initialized_notification());

    // didOpen
    write_jsonrpc_message(
        &mut stdin,
        &did_open_notification(uri.clone(), "java", 1, text),
    );

    // 1) prepareCallHierarchy at `foo`.
    let foo_offset = text.find("foo").expect("foo method");
    let foo_pos = utf16_position(text, foo_offset);
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            call_hierarchy_prepare_params(uri.clone(), foo_pos),
            2,
            "textDocument/prepareCallHierarchy",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let items: Vec<CallHierarchyItem> = serde_json::from_value(
        resp.get("result")
            .cloned()
            .expect("prepareCallHierarchy result"),
    )
    .expect("prepareCallHierarchy result array");
    assert!(
        !items.is_empty(),
        "expected prepareCallHierarchy to return at least one item: {resp:#}"
    );
    assert_eq!(items[0].name, "foo");
    let foo_item = items[0].clone();

    // 2) outgoingCalls for `foo` should include `bar`.
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            call_hierarchy_outgoing_params(foo_item),
            3,
            "callHierarchy/outgoingCalls",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 3);
    let outgoing: Vec<CallHierarchyOutgoingCall> =
        serde_json::from_value(resp.get("result").cloned().expect("outgoingCalls result"))
            .expect("outgoingCalls result array");
    assert!(
        outgoing.iter().any(|call| call.to.name == "bar"),
        "expected outgoing calls to include `bar`, got: {resp:#}"
    );

    // 3) prepareTypeHierarchy at `B`.
    let b_offset = text.find("B extends").expect("class B");
    let b_pos = utf16_position(text, b_offset);
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            type_hierarchy_prepare_params(uri.clone(), b_pos),
            4,
            "textDocument/prepareTypeHierarchy",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 4);
    let items: Vec<TypeHierarchyItem> = serde_json::from_value(
        resp.get("result")
            .cloned()
            .expect("prepareTypeHierarchy result"),
    )
    .expect("prepareTypeHierarchy result array");
    assert!(
        !items.is_empty(),
        "expected prepareTypeHierarchy to return at least one item: {resp:#}"
    );
    assert_eq!(items[0].name, "B");
    let b_item = items[0].clone();

    // 4) supertypes for `B` should include `A`.
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            type_hierarchy_supertypes_params(b_item),
            5,
            "typeHierarchy/supertypes",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 5);
    let supertypes: Vec<TypeHierarchyItem> =
        serde_json::from_value(resp.get("result").cloned().expect("supertypes result"))
            .expect("supertypes result array");
    assert!(
        supertypes.iter().any(|item| item.name == "A"),
        "expected supertypes to include `A`, got: {resp:#}"
    );

    let a_item = supertypes
        .into_iter()
        .find(|item| item.name == "A")
        .expect("expected supertypes to include A");

    // 5) subtypes for `A` should include `B`.
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            type_hierarchy_subtypes_params(a_item),
            6,
            "typeHierarchy/subtypes",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 6);
    let subtypes: Vec<TypeHierarchyItem> =
        serde_json::from_value(resp.get("result").cloned().expect("subtypes result"))
            .expect("subtypes result array");
    assert!(
        subtypes.iter().any(|item| item.name == "B"),
        "expected subtypes to include `B`, got: {resp:#}"
    );

    // shutdown + exit
    write_jsonrpc_message(&mut stdin, &shutdown_request(7));
    let _shutdown_resp = read_response_with_id(&mut stdout, 7);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
