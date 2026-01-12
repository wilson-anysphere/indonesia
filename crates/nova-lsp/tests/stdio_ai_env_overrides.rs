use lsp_types::{CompletionList, Position, Uri};
use nova_lsp::MoreCompletionsResult;
use serde_json::json;
use std::fs;
use std::io::BufReader;
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::time::Duration;
use tempfile::TempDir;

mod support;

fn run_completion_request_with_env(env_key: &str) {
    let _lock = support::stdio_server_lock();
    let completion_payload = r#"
    {
      "completions": [
        {
          "label": "should not be requested",
          "insert_text": "unused()",
          "format": "plain",
          "additional_edits": [],
          "confidence": 0.9
        }
      ]
    }
    "#;

    let ai_server = support::TestAiServer::start(json!({ "completion": completion_payload }));
    let endpoint = format!("{}/complete", ai_server.base_url());

    let temp = TempDir::new().expect("tempdir");
    let config_path = temp.path().join("nova.toml");
    fs::write(
        &config_path,
        format!(
            r#"
[ai]
enabled = true

[ai.features]
multi_token_completion = true

[ai.provider]
kind = "http"
url = "{endpoint}"
model = "default"
"#
        ),
    )
    .expect("write config");

    let file_path = temp.path().join("Foo.java");
    let source = concat!(
        "package com.example;\n",
        "\n",
        "import java.util.List;\n",
        "import java.util.stream.Stream;\n",
        "\n",
        "class Foo {\n",
        "    void test() {\n",
        "        Stream stream = List.of(1).stream();\n",
        "        stream.\n",
        "    }\n",
        "}\n"
    );
    fs::write(&file_path, source).expect("write Foo.java");
    let uri = Uri::from_str(&format!("file://{}", file_path.to_string_lossy())).expect("uri");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        .env(env_key, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "capabilities": {} }
        }),
    );
    let _initialize_resp = support::read_response_with_id(&mut stdout, 1);
    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": { "textDocument": { "uri": uri, "languageId": "java", "version": 1, "text": source } }
        }),
    );

    let cursor = Position::new(8, 15); // end of "stream."
    support::write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/completion",
            "params": {
                "textDocument": { "uri": uri },
                "position": cursor
            }
        }),
    );
    let completion_resp = support::read_response_with_id(&mut stdout, 2);
    let completion_result = completion_resp
        .get("result")
        .cloned()
        .expect("completion result");
    let list: CompletionList = serde_json::from_value(completion_result).expect("decode completion list");
    assert_eq!(
        list.is_incomplete, false,
        "expected no AI completions when {env_key} is set"
    );

    let context_id = list
        .items
        .iter()
        .find_map(|item| {
            item.data
                .as_ref()
                .and_then(|data| data.get("nova"))
                .and_then(|nova| nova.get("completion_context_id"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .expect("completion_context_id present on at least one item");

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "nova/completion/more",
            "params": { "context_id": context_id }
        }),
    );
    let more_resp = support::read_response_with_id(&mut stdout, 3);
    let more_result = more_resp.get("result").cloned().expect("result");
    let more: MoreCompletionsResult = serde_json::from_value(more_result).expect("decode more completions");
    assert!(!more.is_incomplete);
    assert!(more.items.is_empty());

    // Best-effort: give any background tasks a chance to misbehave and hit the provider.
    std::thread::sleep(Duration::from_millis(100));

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 4, "method": "shutdown" }),
    );
    let _shutdown_resp = support::read_response_with_id(&mut stdout, 4);
    support::write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());

    ai_server.assert_hits(0);
}

#[test]
fn stdio_server_honors_nova_disable_ai_env_var() {
    run_completion_request_with_env("NOVA_DISABLE_AI");
}

#[test]
fn stdio_server_honors_nova_disable_ai_completions_env_var() {
    run_completion_request_with_env("NOVA_DISABLE_AI_COMPLETIONS");
}

