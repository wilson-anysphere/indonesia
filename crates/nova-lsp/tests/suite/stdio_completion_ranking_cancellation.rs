use lsp_types::Position;
use serde_json::json;
use std::fs;
use std::io::BufReader;
use std::process::{Command, Stdio};
use tempfile::TempDir;

use crate::support::{file_uri, read_response_with_id, write_jsonrpc_message};

#[test]
fn stdio_server_does_not_start_ai_completion_ranking_work_when_cancelled() {
    let _lock = crate::support::stdio_server_lock();

    let ai_server = crate::support::TestAiServer::start(json!({ "completion": "unused" }));
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
completion_ranking = true

[ai.provider]
kind = "http"
url = "{endpoint}"
model = "default"
"#
        ),
    )
    .expect("write config");

    // Make the file large enough that the cancellation notification is very likely to be processed
    // before any completion-ranking work begins.
    let filler = "        int x = 0;\n".repeat(10_000);
    let source = format!(
        "package com.example;\n\nclass Foo {{\n    void test() {{\n        System.out.pr\n{filler}    }}\n}}\n"
    );

    let file_path = temp.path().join("Foo.java");
    fs::write(&file_path, &source).expect("write Foo.java");
    let uri = file_uri(&file_path);

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        // Ensure a developer's environment doesn't disable AI for this test.
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .env_remove("NOVA_AI_COMPLETIONS_MAX_ITEMS")
        // Ensure legacy AI env vars cannot override the config file.
        .env_remove("NOVA_AI_PROVIDER")
        .env_remove("NOVA_AI_ENDPOINT")
        .env_remove("NOVA_AI_MODEL")
        .env_remove("NOVA_AI_API_KEY")
        .env_remove("NOVA_AI_AUDIT_LOGGING")
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
            "params": { "capabilities": {} }
        }),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
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
                    "uri": uri,
                    "languageId": "java",
                    "version": 1,
                    "text": source
                }
            }
        }),
    );

    // 3) request completion then cancel it immediately.
    let cursor = Position::new(4, 21); // end of "System.out.pr"
    write_jsonrpc_message(
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
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "$/cancelRequest",
            "params": { "id": 2 }
        }),
    );

    let _completion_resp = read_response_with_id(&mut stdout, 2);

    // Give the server a brief chance to issue any unexpected provider requests.
    std::thread::sleep(std::time::Duration::from_millis(50));
    ai_server.assert_hits(0);

    // 4) shutdown + exit
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 3, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
