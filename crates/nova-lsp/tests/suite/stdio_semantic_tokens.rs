use nova_core::{path_to_file_uri, AbsPathBuf};
use serde_json::json;
use std::io::BufReader;
use std::path::Path;
use std::process::{Command, Stdio};
use tempfile::TempDir;

use crate::support;
use support::{read_response_with_id, stdio_server_lock, write_jsonrpc_message};

fn uri_for_path(path: &Path) -> String {
    let abs = AbsPathBuf::try_from(path.to_path_buf()).expect("abs path");
    path_to_file_uri(&abs).expect("file uri")
}

#[test]
fn stdio_server_supports_semantic_tokens_full_delta() {
    let _lock = stdio_server_lock();

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();

    let cache_dir = TempDir::new().expect("cache dir");

    let file_path = root.join("Foo.java");
    let file_uri = uri_for_path(&file_path);
    let root_uri = uri_for_path(root);

    let text = r#"
        public class Foo {
            int field;
            void bar(int a) {
                int b = 1;
                System.out.println(a + b);
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

    // 1) initialize
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
            .pointer("/result/capabilities/semanticTokensProvider")
            .is_some(),
        "expected semanticTokensProvider capability"
    );
    let token_types = initialize_resp
        .pointer("/result/capabilities/semanticTokensProvider/legend/tokenTypes")
        .and_then(|v| v.as_array())
        .expect("legend tokenTypes array");
    assert!(
        !token_types.is_empty(),
        "expected semanticTokens legend tokenTypes to be non-empty"
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    // 2) open document
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": file_uri,
                    "languageId": "java",
                    "version": 1,
                    "text": text,
                }
            }
        }),
    );

    // 3) request full tokens
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/semanticTokens/full",
            "params": { "textDocument": { "uri": file_uri } }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let data = resp
        .pointer("/result/data")
        .and_then(|v| v.as_array())
        .expect("semantic tokens result.data array");
    assert!(
        !data.is_empty(),
        "expected non-empty semantic tokens result.data"
    );

    let result_id = resp
        .pointer("/result/resultId")
        .and_then(|v| v.as_str())
        .expect("semantic tokens resultId")
        .to_string();

    // 4) request delta
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "textDocument/semanticTokens/full/delta",
            "params": {
                "textDocument": { "uri": file_uri },
                "previousResultId": result_id,
            }
        }),
    );
    let delta_resp = read_response_with_id(&mut stdout, 3);
    let delta_result = delta_resp
        .get("result")
        .expect("semantic tokens delta result");

    if let Some(data) = delta_result.get("data").and_then(|v| v.as_array()) {
        assert!(
            !data.is_empty(),
            "expected non-empty semantic tokens delta result.data"
        );
    } else if delta_result
        .get("edits")
        .and_then(|v| v.as_array())
        .is_some()
    {
        // Accept a delta response (edits may be empty when the token stream is unchanged).
        assert!(
            delta_result.get("resultId").is_some(),
            "expected semantic tokens delta resultId"
        );
    } else {
        panic!("unexpected semantic tokens delta response: {delta_resp}");
    }

    // 5) shutdown + exit
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
