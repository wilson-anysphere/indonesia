use nova_core::{path_to_file_uri, AbsPathBuf, LineIndex, TextSize};
use serde_json::json;
use std::io::BufReader;
use std::path::Path;
use std::process::{Command, Stdio};
use tempfile::TempDir;

use crate::support::{read_response_with_id, stdio_server_lock, write_jsonrpc_message};

fn uri_for_path(path: &Path) -> String {
    let abs = AbsPathBuf::try_from(path.to_path_buf()).expect("abs path");
    path_to_file_uri(&abs).expect("file uri")
}

fn utf16_position(text: &str, offset: usize) -> nova_core::Position {
    let index = LineIndex::new(text);
    let offset = TextSize::from(u32::try_from(offset).expect("offset fits u32"));
    index.position(text, offset)
}

#[test]
fn stdio_server_supports_call_hierarchy_outgoing_calls() {
    let _lock = stdio_server_lock();

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();

    let cache_dir = TempDir::new().expect("cache dir");

    let file_path = root.join("Foo.java");
    let file_uri = uri_for_path(&file_path);
    let root_uri = uri_for_path(root);

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
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "rootUri": root_uri, "capabilities": {} }
        }),
    );
    let initialize_resp = read_response_with_id(&mut stdout, 1);
    assert!(
        initialize_resp
            .pointer("/result/capabilities/callHierarchyProvider")
            .is_some(),
        "expected callHierarchyProvider capability"
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    // didOpen
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": file_uri.as_str(),
                    "languageId": "java",
                    "version": 1,
                    "text": text,
                }
            }
        }),
    );

    // prepareCallHierarchy at the caller method name.
    let caller_offset = text.find("caller").expect("caller method name");
    let pos = utf16_position(text, caller_offset);
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/prepareCallHierarchy",
            "params": {
                "textDocument": { "uri": file_uri.as_str() },
                "position": { "line": pos.line, "character": pos.character },
            }
        }),
    );

    let prepare_resp = read_response_with_id(&mut stdout, 2);
    let items = prepare_resp
        .get("result")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("expected prepareCallHierarchy result array: {prepare_resp:#}"));
    assert!(
        !items.is_empty(),
        "expected non-empty prepareCallHierarchy result: {prepare_resp:#}"
    );
    let item = items[0].clone();

    // outgoingCalls should contain `callee`.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "callHierarchy/outgoingCalls",
            "params": { "item": item }
        }),
    );
    let outgoing_resp = read_response_with_id(&mut stdout, 3);
    let outgoing = outgoing_resp
        .get("result")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("expected outgoingCalls result array: {outgoing_resp:#}"));
    assert!(
        outgoing.iter().any(|value| {
            value
                .pointer("/to/name")
                .and_then(|v| v.as_str())
                .is_some_and(|name| name == "callee")
        }),
        "expected outgoing calls to include callee: {outgoing_resp:#}"
    );

    // incomingCalls for `callee` should include `caller`.
    let callee_item = outgoing
        .iter()
        .find(|value| {
            value
                .pointer("/to/name")
                .and_then(|v| v.as_str())
                .is_some_and(|name| name == "callee")
        })
        .and_then(|value| value.get("to"))
        .cloned()
        .unwrap_or_else(|| {
            panic!("expected outgoingCalls to include callee item: {outgoing_resp:#}")
        });

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "callHierarchy/incomingCalls",
            "params": { "item": callee_item }
        }),
    );
    let incoming_resp = read_response_with_id(&mut stdout, 4);
    let incoming = incoming_resp
        .get("result")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("expected incomingCalls result array: {incoming_resp:#}"));
    assert!(
        incoming.iter().any(|value| {
            value
                .pointer("/from/name")
                .and_then(|v| v.as_str())
                .is_some_and(|name| name == "caller")
        }),
        "expected incoming calls to include caller: {incoming_resp:#}"
    );

    // shutdown + exit
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 5, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 5);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
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
    let root_uri = uri_for_path(root);

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

    // didOpen
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": file_uri.as_str(),
                    "languageId": "java",
                    "version": 1,
                    "text": text,
                }
            }
        }),
    );

    // Prepare + outgoing for `foo(int)`: should call `bar`, not `baz`.
    let foo_int_offset = text.find("foo(int").expect("foo(int)");
    let pos = utf16_position(text, foo_int_offset);
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/prepareCallHierarchy",
            "params": {
                "textDocument": { "uri": file_uri.as_str() },
                "position": { "line": pos.line, "character": pos.character },
            }
        }),
    );
    let prepare_int = read_response_with_id(&mut stdout, 2);
    let items = prepare_int
        .get("result")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("expected prepareCallHierarchy result array: {prepare_int:#}"));
    assert_eq!(
        items.len(),
        1,
        "expected one call hierarchy item: {prepare_int:#}"
    );
    let foo_int_item = items[0].clone();
    assert!(
        foo_int_item
            .pointer("/detail")
            .and_then(|v| v.as_str())
            .is_some_and(|detail| detail.contains("foo(") && detail.contains("int")),
        "expected foo(int) to include signature detail: {foo_int_item:#}"
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "callHierarchy/outgoingCalls",
            "params": { "item": foo_int_item }
        }),
    );
    let outgoing_int_resp = read_response_with_id(&mut stdout, 3);
    let outgoing_int = outgoing_int_resp
        .get("result")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("expected outgoingCalls result array: {outgoing_int_resp:#}"));
    assert!(
        outgoing_int.iter().any(|value| {
            value
                .pointer("/to/name")
                .and_then(|v| v.as_str())
                .is_some_and(|name| name == "bar")
        }),
        "expected foo(int) outgoing calls to include bar: {outgoing_int_resp:#}"
    );
    assert!(
        !outgoing_int.iter().any(|value| {
            value
                .pointer("/to/name")
                .and_then(|v| v.as_str())
                .is_some_and(|name| name == "baz")
        }),
        "expected foo(int) outgoing calls to exclude baz: {outgoing_int_resp:#}"
    );

    // Prepare + outgoing for `foo(String)`: should call `baz`, not `bar`.
    let foo_string_offset = text.find("foo(String").expect("foo(String)");
    let pos = utf16_position(text, foo_string_offset);
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "textDocument/prepareCallHierarchy",
            "params": {
                "textDocument": { "uri": file_uri.as_str() },
                "position": { "line": pos.line, "character": pos.character },
            }
        }),
    );
    let prepare_string = read_response_with_id(&mut stdout, 4);
    let items = prepare_string
        .get("result")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| {
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
            .pointer("/detail")
            .and_then(|v| v.as_str())
            .is_some_and(|detail| detail.contains("foo(") && detail.contains("String")),
        "expected foo(String) to include signature detail: {foo_string_item:#}"
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "callHierarchy/outgoingCalls",
            "params": { "item": foo_string_item }
        }),
    );
    let outgoing_string_resp = read_response_with_id(&mut stdout, 5);
    let outgoing_string = outgoing_string_resp
        .get("result")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("expected outgoingCalls result array: {outgoing_string_resp:#}"));
    assert!(
        outgoing_string.iter().any(|value| {
            value
                .pointer("/to/name")
                .and_then(|v| v.as_str())
                .is_some_and(|name| name == "baz")
        }),
        "expected foo(String) outgoing calls to include baz: {outgoing_string_resp:#}"
    );
    assert!(
        !outgoing_string.iter().any(|value| {
            value
                .pointer("/to/name")
                .and_then(|v| v.as_str())
                .is_some_and(|name| name == "bar")
        }),
        "expected foo(String) outgoing calls to exclude bar: {outgoing_string_resp:#}"
    );

    // shutdown + exit
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 6, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 6);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
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
    let root_uri = uri_for_path(root);

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

    // didOpen both files
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": foo_uri.as_str(),
                    "languageId": "java",
                    "version": 1,
                    "text": foo_text,
                }
            }
        }),
    );
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": bar_uri.as_str(),
                    "languageId": "java",
                    "version": 1,
                    "text": bar_text,
                }
            }
        }),
    );

    // prepareCallHierarchy at the caller method name.
    let caller_offset = foo_text.find("caller").expect("caller method name");
    let pos = utf16_position(foo_text, caller_offset);
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/prepareCallHierarchy",
            "params": {
                "textDocument": { "uri": foo_uri.as_str() },
                "position": { "line": pos.line, "character": pos.character },
            }
        }),
    );

    let prepare_resp = read_response_with_id(&mut stdout, 2);
    let items = prepare_resp
        .get("result")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("expected prepareCallHierarchy result array: {prepare_resp:#}"));
    assert!(
        !items.is_empty(),
        "expected non-empty prepareCallHierarchy result: {prepare_resp:#}"
    );
    let item = items[0].clone();

    // outgoingCalls should contain Bar.callee().
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "callHierarchy/outgoingCalls",
            "params": { "item": item }
        }),
    );
    let outgoing_resp = read_response_with_id(&mut stdout, 3);
    let outgoing = outgoing_resp
        .get("result")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("expected outgoingCalls result array: {outgoing_resp:#}"));

    let callee_call = outgoing
        .iter()
        .find(|value| {
            value
                .pointer("/to/name")
                .and_then(|v| v.as_str())
                .is_some_and(|name| name == "callee")
                && value
                    .pointer("/to/uri")
                    .and_then(|v| v.as_str())
                    .is_some_and(|uri| uri == bar_uri.as_str())
        })
        .unwrap_or_else(|| {
            panic!("expected outgoing calls to include Bar.callee: {outgoing_resp:#}")
        });

    assert!(
        callee_call
            .pointer("/fromRanges")
            .and_then(|v| v.as_array())
            .is_some_and(|ranges| !ranges.is_empty()),
        "expected outgoing call to include fromRanges: {outgoing_resp:#}"
    );

    let callee_item = callee_call
        .get("to")
        .cloned()
        .expect("callee call should have `to` CallHierarchyItem");

    assert!(
        callee_item
            .pointer("/detail")
            .and_then(|v| v.as_str())
            .is_some_and(|detail| detail.contains("callee(")),
        "expected Bar.callee CallHierarchyItem to include detail: {callee_item:#}"
    );

    // incomingCalls on Bar.callee should include Foo.caller.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "callHierarchy/incomingCalls",
            "params": { "item": callee_item }
        }),
    );
    let incoming_resp = read_response_with_id(&mut stdout, 4);
    let incoming = incoming_resp
        .get("result")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("expected incomingCalls result array: {incoming_resp:#}"));
    assert!(
        incoming.iter().any(|value| {
            value
                .pointer("/from/name")
                .and_then(|v| v.as_str())
                .is_some_and(|name| name == "caller")
                && value
                    .pointer("/from/uri")
                    .and_then(|v| v.as_str())
                    .is_some_and(|uri| uri == foo_uri.as_str())
        }),
        "expected incoming calls to include Foo.caller: {incoming_resp:#}"
    );

    // shutdown + exit
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 5, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 5);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
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
    let root_uri = uri_for_path(root);

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

    // didOpen both files
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": foo_uri.as_str(),
                    "languageId": "java",
                    "version": 1,
                    "text": foo_text,
                }
            }
        }),
    );
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": bar_uri.as_str(),
                    "languageId": "java",
                    "version": 1,
                    "text": bar_text,
                }
            }
        }),
    );

    // prepareCallHierarchy at the call-site name (`Bar.callee()`).
    let callee_offset = foo_text.find("callee").expect("callee call-site name");
    let pos = utf16_position(foo_text, callee_offset);
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/prepareCallHierarchy",
            "params": {
                "textDocument": { "uri": foo_uri.as_str() },
                "position": { "line": pos.line, "character": pos.character },
            }
        }),
    );

    let prepare_resp = read_response_with_id(&mut stdout, 2);
    let items = prepare_resp
        .get("result")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("expected prepareCallHierarchy result array: {prepare_resp:#}"));
    assert!(
        !items.is_empty(),
        "expected non-empty prepareCallHierarchy result: {prepare_resp:#}"
    );

    let callee_item = items
        .iter()
        .find(|value| {
            value
                .pointer("/name")
                .and_then(|v| v.as_str())
                .is_some_and(|name| name == "callee")
                && value
                    .pointer("/uri")
                    .and_then(|v| v.as_str())
                    .is_some_and(|uri| uri == bar_uri.as_str())
        })
        .cloned()
        .unwrap_or_else(|| {
            panic!("expected prepareCallHierarchy to resolve Bar.callee: {prepare_resp:#}")
        });

    assert!(
        callee_item
            .pointer("/detail")
            .and_then(|v| v.as_str())
            .is_some_and(|detail| detail.contains("callee(")),
        "expected prepareCallHierarchy item to include detail: {callee_item:#}"
    );

    // incomingCalls on Bar.callee should include Foo.caller.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "callHierarchy/incomingCalls",
            "params": { "item": callee_item }
        }),
    );
    let incoming_resp = read_response_with_id(&mut stdout, 3);
    let incoming = incoming_resp
        .get("result")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("expected incomingCalls result array: {incoming_resp:#}"));
    let caller_call = incoming
        .iter()
        .find(|value| {
            value
                .pointer("/from/name")
                .and_then(|v| v.as_str())
                .is_some_and(|name| name == "caller")
                && value
                    .pointer("/from/uri")
                    .and_then(|v| v.as_str())
                    .is_some_and(|uri| uri == foo_uri.as_str())
        })
        .unwrap_or_else(|| {
            panic!("expected incoming calls to include Foo.caller: {incoming_resp:#}")
        });

    assert!(
        caller_call
            .pointer("/from/detail")
            .and_then(|v| v.as_str())
            .is_some_and(|detail| detail.contains("caller(")),
        "expected Foo.caller CallHierarchyItem to include detail: {caller_call:#}"
    );

    // shutdown + exit
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 4, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 4);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
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
    let root_uri = uri_for_path(root);

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

    // didOpen both files
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": a_uri.as_str(),
                    "languageId": "java",
                    "version": 1,
                    "text": a_text,
                }
            }
        }),
    );
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": b_uri.as_str(),
                    "languageId": "java",
                    "version": 1,
                    "text": b_text,
                }
            }
        }),
    );

    // prepareCallHierarchy at the receiverless inherited call-site name (`bar()`).
    let bar_offset = b_text.find("bar();").expect("bar call-site");
    let pos = utf16_position(b_text, bar_offset);
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/prepareCallHierarchy",
            "params": {
                "textDocument": { "uri": b_uri.as_str() },
                "position": { "line": pos.line, "character": pos.character },
            }
        }),
    );

    let prepare_resp = read_response_with_id(&mut stdout, 2);
    let items = prepare_resp
        .get("result")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("expected prepareCallHierarchy result array: {prepare_resp:#}"));
    assert!(
        !items.is_empty(),
        "expected non-empty prepareCallHierarchy result: {prepare_resp:#}"
    );

    let bar_item = items
        .iter()
        .find(|value| {
            value
                .pointer("/name")
                .and_then(|v| v.as_str())
                .is_some_and(|name| name == "bar")
                && value
                    .pointer("/uri")
                    .and_then(|v| v.as_str())
                    .is_some_and(|uri| uri == a_uri.as_str())
        })
        .cloned()
        .unwrap_or_else(|| {
            panic!("expected prepareCallHierarchy to resolve inherited A.bar: {prepare_resp:#}")
        });

    assert!(
        bar_item
            .pointer("/detail")
            .and_then(|v| v.as_str())
            .is_some_and(|detail| detail.contains("bar(")),
        "expected A.bar CallHierarchyItem to include detail: {bar_item:#}"
    );

    // incomingCalls on A.bar should include B.foo with the bar() call site in fromRanges.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "callHierarchy/incomingCalls",
            "params": { "item": bar_item }
        }),
    );

    let incoming_resp = read_response_with_id(&mut stdout, 3);
    let incoming = incoming_resp
        .get("result")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("expected incomingCalls result array: {incoming_resp:#}"));

    let foo_call = incoming
        .iter()
        .find(|value| {
            value
                .pointer("/from/name")
                .and_then(|v| v.as_str())
                .is_some_and(|name| name == "foo")
                && value
                    .pointer("/from/uri")
                    .and_then(|v| v.as_str())
                    .is_some_and(|uri| uri == b_uri.as_str())
        })
        .unwrap_or_else(|| panic!("expected incoming calls to include B.foo: {incoming_resp:#}"));

    assert!(
        foo_call
            .pointer("/from/detail")
            .and_then(|v| v.as_str())
            .is_some_and(|detail| detail.contains("foo(")),
        "expected B.foo CallHierarchyItem to include detail: {foo_call:#}"
    );

    let ranges = foo_call
        .pointer("/fromRanges")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| {
            panic!("expected incoming call to include fromRanges: {incoming_resp:#}")
        });
    assert!(
        !ranges.is_empty(),
        "expected incoming call to include non-empty fromRanges: {incoming_resp:#}"
    );

    let expected_start = utf16_position(b_text, bar_offset);
    let expected_end = utf16_position(b_text, bar_offset + "bar".len());
    assert!(
        ranges.iter().any(|range| {
            range
                .pointer("/start/line")
                .and_then(|v| v.as_u64())
                .is_some_and(|line| line == expected_start.line as u64)
                && range
                    .pointer("/start/character")
                    .and_then(|v| v.as_u64())
                    .is_some_and(|ch| ch == expected_start.character as u64)
                && range
                    .pointer("/end/line")
                    .and_then(|v| v.as_u64())
                    .is_some_and(|line| line == expected_end.line as u64)
                && range
                    .pointer("/end/character")
                    .and_then(|v| v.as_u64())
                    .is_some_and(|ch| ch == expected_end.character as u64)
        }),
        "expected fromRanges to include the bar() call-site range: {ranges:#?}"
    );

    // shutdown + exit
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 4, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 4);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
