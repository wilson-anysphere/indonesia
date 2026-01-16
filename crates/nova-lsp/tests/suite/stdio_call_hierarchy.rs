use lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams, CallHierarchyItem,
    CallHierarchyOutgoingCall, CallHierarchyOutgoingCallsParams, CallHierarchyPrepareParams,
    InitializeResult, PartialResultParams, Position, Range, TextDocumentIdentifier,
    TextDocumentPositionParams, Uri, WorkDoneProgressParams,
};
use nova_core::{path_to_file_uri, AbsPathBuf, LineIndex, TextSize};
use std::io::BufReader;
use std::path::Path;
use std::process::{Command, Stdio};
use tempfile::TempDir;

use crate::support::{
    did_open_notification, exit_notification, initialize_request_with_root_uri,
    initialized_notification, jsonrpc_request, read_response_with_id, shutdown_request,
    stdio_server_lock, write_jsonrpc_message,
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
    let offset = TextSize::from(u32::try_from(offset).expect("offset fits u32"));
    index.position(text, offset)
}

fn prepare_params(uri: Uri, pos: nova_core::Position) -> CallHierarchyPrepareParams {
    CallHierarchyPrepareParams {
        text_document_position_params: TextDocumentPositionParams::new(
            TextDocumentIdentifier { uri },
            Position::new(pos.line, pos.character),
        ),
        work_done_progress_params: WorkDoneProgressParams::default(),
    }
}

fn outgoing_params(item: CallHierarchyItem) -> CallHierarchyOutgoingCallsParams {
    CallHierarchyOutgoingCallsParams {
        item,
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    }
}

fn incoming_params(item: CallHierarchyItem) -> CallHierarchyIncomingCallsParams {
    CallHierarchyIncomingCallsParams {
        item,
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    }
}

#[test]
fn stdio_server_supports_call_hierarchy_outgoing_calls() {
    let _lock = stdio_server_lock();

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();

    let cache_dir = TempDir::new().expect("cache dir");

    let file_path = root.join("Foo.java");
    let file_uri = uri_for_path(&file_path);
    let root_uri = uri_for_path(root).as_str().to_string();

    let text = r#"
        public class Foo {
            void caller() {
                callee();
            }

            void callee() {}
        }
    "#;

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

    // initialize
    write_jsonrpc_message(&mut stdin, &initialize_request_with_root_uri(1, root_uri));
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

    write_jsonrpc_message(&mut stdin, &initialized_notification());

    // didOpen
    write_jsonrpc_message(
        &mut stdin,
        &did_open_notification(file_uri.clone(), "java", 1, text),
    );

    // prepareCallHierarchy at the caller method name.
    let caller_offset = text.find("caller").expect("caller method name");
    let pos = utf16_position(text, caller_offset);
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            prepare_params(file_uri.clone(), pos),
            2,
            "textDocument/prepareCallHierarchy",
        ),
    );

    let prepare_resp = read_response_with_id(&mut stdout, 2);
    let items: Vec<CallHierarchyItem> =
        serde_json::from_value(prepare_resp.get("result").cloned().unwrap_or_default())
            .unwrap_or_else(|_| {
                panic!("expected prepareCallHierarchy result array: {prepare_resp:#}")
            });
    assert!(
        !items.is_empty(),
        "expected non-empty prepareCallHierarchy result: {prepare_resp:#}"
    );
    let item = items[0].clone();

    // outgoingCalls should contain `callee`.
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(outgoing_params(item), 3, "callHierarchy/outgoingCalls"),
    );
    let outgoing_resp = read_response_with_id(&mut stdout, 3);
    let outgoing: Vec<CallHierarchyOutgoingCall> =
        serde_json::from_value(outgoing_resp.get("result").cloned().unwrap_or_default())
            .unwrap_or_else(|_| panic!("expected outgoingCalls result array: {outgoing_resp:#}"));
    assert!(
        outgoing.iter().any(|call| call.to.name == "callee"),
        "expected outgoing calls to include callee: {outgoing_resp:#}"
    );

    // incomingCalls for `callee` should include `caller`.
    let callee_item = outgoing
        .iter()
        .find(|call| call.to.name == "callee")
        .map(|call| call.to.clone())
        .unwrap_or_else(|| {
            panic!("expected outgoingCalls to include callee item: {outgoing_resp:#}")
        });

    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            incoming_params(callee_item),
            4,
            "callHierarchy/incomingCalls",
        ),
    );
    let incoming_resp = read_response_with_id(&mut stdout, 4);
    let incoming: Vec<CallHierarchyIncomingCall> =
        serde_json::from_value(incoming_resp.get("result").cloned().unwrap_or_default())
            .unwrap_or_else(|_| panic!("expected incomingCalls result array: {incoming_resp:#}"));
    assert!(
        incoming.iter().any(|call| call.from.name == "caller"),
        "expected incoming calls to include caller: {incoming_resp:#}"
    );

    // shutdown + exit
    write_jsonrpc_message(&mut stdin, &shutdown_request(5));
    let _shutdown_resp = read_response_with_id(&mut stdout, 5);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_call_hierarchy_outgoing_calls_disambiguates_overloads() {
    let _lock = stdio_server_lock();

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();

    let cache_dir = TempDir::new().expect("cache dir");

    let file_path = root.join("Foo.java");
    let file_uri = uri_for_path(&file_path);
    let root_uri = uri_for_path(root).as_str().to_string();

    let text = r#"
        public class Foo {
            void bar() {}
            void baz() {}

            void foo(int x) {
                bar();
            }

            void foo(String s) {
                baz();
            }
        }
    "#;

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

    // initialize
    write_jsonrpc_message(&mut stdin, &initialize_request_with_root_uri(1, root_uri));
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &initialized_notification());

    // didOpen
    write_jsonrpc_message(
        &mut stdin,
        &did_open_notification(file_uri.clone(), "java", 1, text),
    );

    // Prepare + outgoing for `foo(int)`: should call `bar`, not `baz`.
    let foo_int_offset = text.find("foo(int").expect("foo(int)");
    let pos = utf16_position(text, foo_int_offset);
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            prepare_params(file_uri.clone(), pos),
            2,
            "textDocument/prepareCallHierarchy",
        ),
    );
    let prepare_int = read_response_with_id(&mut stdout, 2);
    let items: Vec<CallHierarchyItem> =
        serde_json::from_value(prepare_int.get("result").cloned().unwrap_or_default())
            .unwrap_or_else(|_| {
                panic!("expected prepareCallHierarchy result array: {prepare_int:#}")
            });
    assert_eq!(
        items.len(),
        1,
        "expected one call hierarchy item: {prepare_int:#}"
    );
    let foo_int_item = items[0].clone();
    assert!(
        foo_int_item
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("foo(") && detail.contains("int")),
        "expected foo(int) to include signature detail: {foo_int_item:#?}"
    );

    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            outgoing_params(foo_int_item),
            3,
            "callHierarchy/outgoingCalls",
        ),
    );
    let outgoing_int_resp = read_response_with_id(&mut stdout, 3);
    let outgoing_int: Vec<CallHierarchyOutgoingCall> =
        serde_json::from_value(outgoing_int_resp.get("result").cloned().unwrap_or_default())
            .unwrap_or_else(|_| {
                panic!("expected outgoingCalls result array: {outgoing_int_resp:#}")
            });
    assert!(
        outgoing_int.iter().any(|call| call.to.name == "bar"),
        "expected foo(int) outgoing calls to include bar: {outgoing_int_resp:#}"
    );
    assert!(
        !outgoing_int.iter().any(|call| call.to.name == "baz"),
        "expected foo(int) outgoing calls to exclude baz: {outgoing_int_resp:#}"
    );

    // Prepare + outgoing for `foo(String)`: should call `baz`, not `bar`.
    let foo_string_offset = text.find("foo(String").expect("foo(String)");
    let pos = utf16_position(text, foo_string_offset);
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            prepare_params(file_uri.clone(), pos),
            4,
            "textDocument/prepareCallHierarchy",
        ),
    );
    let prepare_string = read_response_with_id(&mut stdout, 4);
    let items: Vec<CallHierarchyItem> =
        serde_json::from_value(prepare_string.get("result").cloned().unwrap_or_default())
            .unwrap_or_else(|_| {
                panic!("expected prepareCallHierarchy result array: {prepare_string:#}")
            });
    assert_eq!(
        items.len(),
        1,
        "expected one call hierarchy item: {prepare_string:#}"
    );
    let foo_string_item = items[0].clone();
    assert!(
        foo_string_item
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("foo(") && detail.contains("String")),
        "expected foo(String) to include signature detail: {foo_string_item:#?}"
    );

    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            outgoing_params(foo_string_item),
            5,
            "callHierarchy/outgoingCalls",
        ),
    );
    let outgoing_string_resp = read_response_with_id(&mut stdout, 5);
    let outgoing_string: Vec<CallHierarchyOutgoingCall> = serde_json::from_value(
        outgoing_string_resp
            .get("result")
            .cloned()
            .unwrap_or_default(),
    )
    .unwrap_or_else(|_| panic!("expected outgoingCalls result array: {outgoing_string_resp:#}"));
    assert!(
        outgoing_string.iter().any(|call| call.to.name == "baz"),
        "expected foo(String) outgoing calls to include baz: {outgoing_string_resp:#}"
    );
    assert!(
        !outgoing_string.iter().any(|call| call.to.name == "bar"),
        "expected foo(String) outgoing calls to exclude bar: {outgoing_string_resp:#}"
    );

    // shutdown + exit
    write_jsonrpc_message(&mut stdin, &shutdown_request(6));
    let _shutdown_resp = read_response_with_id(&mut stdout, 6);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_call_hierarchy_incoming_calls_for_overloads_is_non_empty() {
    let _lock = stdio_server_lock();

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();

    let cache_dir = TempDir::new().expect("cache dir");

    let file_path = root.join("Foo.java");
    let file_uri = uri_for_path(&file_path);
    let root_uri = uri_for_path(root).as_str().to_string();

    let text = r#"
        public class Foo {
            void bar(int x) {}
            void bar(String s) {}

            void callInt() {
                bar(1);
            }

            void callString() {
                bar("x");
            }
        }
    "#;

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

    // initialize
    write_jsonrpc_message(&mut stdin, &initialize_request_with_root_uri(1, root_uri));
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &initialized_notification());

    // didOpen
    write_jsonrpc_message(
        &mut stdin,
        &did_open_notification(file_uri.clone(), "java", 1, text),
    );

    // prepareCallHierarchy on the *second* overload `bar(String)`.
    let bar_string_offset = text.find("bar(String").expect("bar(String)");
    let pos = utf16_position(text, bar_string_offset);
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            prepare_params(file_uri.clone(), pos),
            2,
            "textDocument/prepareCallHierarchy",
        ),
    );
    let prepare_resp = read_response_with_id(&mut stdout, 2);
    let items: Vec<CallHierarchyItem> =
        serde_json::from_value(prepare_resp.get("result").cloned().unwrap_or_default())
            .unwrap_or_else(|_| {
                panic!("expected prepareCallHierarchy result array: {prepare_resp:#}")
            });
    assert_eq!(
        items.len(),
        1,
        "expected one call hierarchy item: {prepare_resp:#}"
    );
    let bar_item = items[0].clone();
    assert!(
        bar_item
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("String")),
        "expected bar(String) to include signature detail: {bar_item:#?}"
    );

    // incomingCalls should include callers (even though overload resolution is best-effort).
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(incoming_params(bar_item), 3, "callHierarchy/incomingCalls"),
    );
    let incoming_resp = read_response_with_id(&mut stdout, 3);
    let incoming: Vec<CallHierarchyIncomingCall> =
        serde_json::from_value(incoming_resp.get("result").cloned().unwrap_or_default())
            .unwrap_or_else(|_| panic!("expected incomingCalls result array: {incoming_resp:#}"));
    assert!(
        incoming.iter().any(|call| call.from.name == "callInt"),
        "expected incoming calls to include callInt: {incoming_resp:#}"
    );
    assert!(
        incoming.iter().any(|call| call.from.name == "callString"),
        "expected incoming calls to include callString: {incoming_resp:#}"
    );

    // shutdown + exit
    write_jsonrpc_message(&mut stdin, &shutdown_request(4));
    let _shutdown_resp = read_response_with_id(&mut stdout, 4);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_supports_call_hierarchy_across_files() {
    let _lock = stdio_server_lock();

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();

    let cache_dir = TempDir::new().expect("cache dir");

    let foo_path = root.join("Foo.java");
    let foo_uri = uri_for_path(&foo_path);
    let bar_path = root.join("Bar.java");
    let bar_uri = uri_for_path(&bar_path);
    let root_uri = uri_for_path(root).as_str().to_string();

    let foo_text = r#"
        public class Foo {
            void caller() {
                Bar.callee();
            }
        }
    "#;

    let bar_text = r#"
        public class Bar {
            static void callee() {}
        }
    "#;

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

    // initialize
    write_jsonrpc_message(&mut stdin, &initialize_request_with_root_uri(1, root_uri));
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &initialized_notification());

    // didOpen both files
    write_jsonrpc_message(
        &mut stdin,
        &did_open_notification(foo_uri.clone(), "java", 1, foo_text),
    );
    write_jsonrpc_message(
        &mut stdin,
        &did_open_notification(bar_uri.clone(), "java", 1, bar_text),
    );

    // prepareCallHierarchy at the caller method name.
    let caller_offset = foo_text.find("caller").expect("caller method name");
    let pos = utf16_position(foo_text, caller_offset);
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            prepare_params(foo_uri.clone(), pos),
            2,
            "textDocument/prepareCallHierarchy",
        ),
    );

    let prepare_resp = read_response_with_id(&mut stdout, 2);
    let items: Vec<CallHierarchyItem> =
        serde_json::from_value(prepare_resp.get("result").cloned().unwrap_or_default())
            .unwrap_or_else(|_| {
                panic!("expected prepareCallHierarchy result array: {prepare_resp:#}")
            });
    assert!(
        !items.is_empty(),
        "expected non-empty prepareCallHierarchy result: {prepare_resp:#}"
    );
    let item = items[0].clone();

    // outgoingCalls should contain Bar.callee().
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(outgoing_params(item), 3, "callHierarchy/outgoingCalls"),
    );
    let outgoing_resp = read_response_with_id(&mut stdout, 3);
    let outgoing: Vec<CallHierarchyOutgoingCall> =
        serde_json::from_value(outgoing_resp.get("result").cloned().unwrap_or_default())
            .unwrap_or_else(|_| panic!("expected outgoingCalls result array: {outgoing_resp:#}"));

    let callee_call = outgoing
        .iter()
        .find(|call| call.to.name == "callee" && call.to.uri == bar_uri)
        .unwrap_or_else(|| {
            panic!("expected outgoing calls to include Bar.callee: {outgoing_resp:#}")
        });

    assert!(
        !callee_call.from_ranges.is_empty(),
        "expected outgoing call to include fromRanges: {outgoing_resp:#}"
    );

    let callee_item = callee_call.to.clone();

    assert!(
        callee_item
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("callee(")),
        "expected Bar.callee CallHierarchyItem to include detail: {callee_item:#?}"
    );

    // incomingCalls on Bar.callee should include Foo.caller.
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            incoming_params(callee_item),
            4,
            "callHierarchy/incomingCalls",
        ),
    );
    let incoming_resp = read_response_with_id(&mut stdout, 4);
    let incoming: Vec<CallHierarchyIncomingCall> =
        serde_json::from_value(incoming_resp.get("result").cloned().unwrap_or_default())
            .unwrap_or_else(|_| panic!("expected incomingCalls result array: {incoming_resp:#}"));
    assert!(
        incoming
            .iter()
            .any(|call| call.from.name == "caller" && call.from.uri == foo_uri),
        "expected incoming calls to include Foo.caller: {incoming_resp:#}"
    );

    // shutdown + exit
    write_jsonrpc_message(&mut stdin, &shutdown_request(5));
    let _shutdown_resp = read_response_with_id(&mut stdout, 5);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_prepare_call_hierarchy_resolves_receiver_call_sites_across_files() {
    let _lock = stdio_server_lock();

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();

    let cache_dir = TempDir::new().expect("cache dir");

    let foo_path = root.join("Foo.java");
    let foo_uri = uri_for_path(&foo_path);
    let bar_path = root.join("Bar.java");
    let bar_uri = uri_for_path(&bar_path);
    let root_uri = uri_for_path(root).as_str().to_string();

    let foo_text = r#"
        public class Foo {
            void caller() {
                Bar.callee();
            }
        }
    "#;

    let bar_text = r#"
        public class Bar {
            static void callee() {}
        }
    "#;

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

    // initialize
    write_jsonrpc_message(&mut stdin, &initialize_request_with_root_uri(1, root_uri));
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &initialized_notification());

    // didOpen both files
    write_jsonrpc_message(
        &mut stdin,
        &did_open_notification(foo_uri.clone(), "java", 1, foo_text),
    );
    write_jsonrpc_message(
        &mut stdin,
        &did_open_notification(bar_uri.clone(), "java", 1, bar_text),
    );

    // prepareCallHierarchy at the call-site name (`Bar.callee()`).
    let callee_offset = foo_text.find("callee").expect("callee call-site name");
    let pos = utf16_position(foo_text, callee_offset);
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            prepare_params(foo_uri.clone(), pos),
            2,
            "textDocument/prepareCallHierarchy",
        ),
    );

    let prepare_resp = read_response_with_id(&mut stdout, 2);
    let items: Vec<CallHierarchyItem> =
        serde_json::from_value(prepare_resp.get("result").cloned().unwrap_or_default())
            .unwrap_or_else(|_| {
                panic!("expected prepareCallHierarchy result array: {prepare_resp:#}")
            });
    assert!(
        !items.is_empty(),
        "expected non-empty prepareCallHierarchy result: {prepare_resp:#}"
    );

    let callee_item = items
        .iter()
        .find(|item| item.name == "callee" && item.uri == bar_uri)
        .cloned()
        .unwrap_or_else(|| {
            panic!("expected prepareCallHierarchy to resolve Bar.callee: {prepare_resp:#}")
        });

    assert!(
        callee_item
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("callee(")),
        "expected prepareCallHierarchy item to include detail: {callee_item:#?}"
    );

    // incomingCalls on Bar.callee should include Foo.caller.
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            incoming_params(callee_item),
            3,
            "callHierarchy/incomingCalls",
        ),
    );
    let incoming_resp = read_response_with_id(&mut stdout, 3);
    let incoming: Vec<CallHierarchyIncomingCall> =
        serde_json::from_value(incoming_resp.get("result").cloned().unwrap_or_default())
            .unwrap_or_else(|_| panic!("expected incomingCalls result array: {incoming_resp:#}"));
    let caller_call = incoming
        .iter()
        .find(|call| call.from.name == "caller" && call.from.uri == foo_uri)
        .unwrap_or_else(|| {
            panic!("expected incoming calls to include Foo.caller: {incoming_resp:#}")
        });

    assert!(
        caller_call
            .from
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("caller(")),
        "expected Foo.caller CallHierarchyItem to include detail: {caller_call:#?}"
    );

    // shutdown + exit
    write_jsonrpc_message(&mut stdin, &shutdown_request(4));
    let _shutdown_resp = read_response_with_id(&mut stdout, 4);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_prepare_call_hierarchy_resolves_inherited_receiverless_call_sites_across_files() {
    let _lock = stdio_server_lock();

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();

    let cache_dir = TempDir::new().expect("cache dir");

    let a_path = root.join("A.java");
    let a_uri = uri_for_path(&a_path);
    let b_path = root.join("B.java");
    let b_uri = uri_for_path(&b_path);
    let root_uri = uri_for_path(root).as_str().to_string();

    let a_text = r#"
        public class A {
            void bar() {}
        }
    "#;

    let b_text = r#"
        public class B extends A {
            void foo() {
                bar();
            }
        }
    "#;

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

    // initialize
    write_jsonrpc_message(&mut stdin, &initialize_request_with_root_uri(1, root_uri));
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &initialized_notification());

    // didOpen both files
    write_jsonrpc_message(
        &mut stdin,
        &did_open_notification(a_uri.clone(), "java", 1, a_text),
    );
    write_jsonrpc_message(
        &mut stdin,
        &did_open_notification(b_uri.clone(), "java", 1, b_text),
    );

    // prepareCallHierarchy at the receiverless inherited call-site name (`bar()`).
    let bar_offset = b_text.find("bar();").expect("bar call-site");
    let pos = utf16_position(b_text, bar_offset);
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            prepare_params(b_uri.clone(), pos),
            2,
            "textDocument/prepareCallHierarchy",
        ),
    );

    let prepare_resp = read_response_with_id(&mut stdout, 2);
    let items: Vec<CallHierarchyItem> =
        serde_json::from_value(prepare_resp.get("result").cloned().unwrap_or_default())
            .unwrap_or_else(|_| {
                panic!("expected prepareCallHierarchy result array: {prepare_resp:#}")
            });
    assert!(
        !items.is_empty(),
        "expected non-empty prepareCallHierarchy result: {prepare_resp:#}"
    );

    let bar_item = items
        .iter()
        .find(|item| item.name == "bar" && item.uri == a_uri)
        .cloned()
        .unwrap_or_else(|| {
            panic!("expected prepareCallHierarchy to resolve inherited A.bar: {prepare_resp:#}")
        });

    assert!(
        bar_item
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("bar(")),
        "expected A.bar CallHierarchyItem to include detail: {bar_item:#?}"
    );

    // incomingCalls on A.bar should include B.foo with the bar() call site in fromRanges.
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(incoming_params(bar_item), 3, "callHierarchy/incomingCalls"),
    );

    let incoming_resp = read_response_with_id(&mut stdout, 3);
    let incoming: Vec<CallHierarchyIncomingCall> =
        serde_json::from_value(incoming_resp.get("result").cloned().unwrap_or_default())
            .unwrap_or_else(|_| panic!("expected incomingCalls result array: {incoming_resp:#}"));

    let foo_call = incoming
        .iter()
        .find(|call| call.from.name == "foo" && call.from.uri == b_uri)
        .unwrap_or_else(|| panic!("expected incoming calls to include B.foo: {incoming_resp:#}"));

    assert!(
        foo_call
            .from
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("foo(")),
        "expected B.foo CallHierarchyItem to include detail: {foo_call:#?}"
    );

    assert!(
        !foo_call.from_ranges.is_empty(),
        "expected incoming call to include non-empty fromRanges: {incoming_resp:#}"
    );

    let expected_start = utf16_position(b_text, bar_offset);
    let expected_end = utf16_position(b_text, bar_offset + "bar".len());
    let expected_range = Range::new(
        Position::new(expected_start.line, expected_start.character),
        Position::new(expected_end.line, expected_end.character),
    );
    assert!(
        foo_call
            .from_ranges
            .iter()
            .any(|range| *range == expected_range),
        "expected fromRanges to include the bar() call-site range: {:#?}",
        foo_call.from_ranges
    );

    // shutdown + exit
    write_jsonrpc_message(&mut stdin, &shutdown_request(4));
    let _shutdown_resp = read_response_with_id(&mut stdout, 4);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
