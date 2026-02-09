use serde_json::json;
use std::fs;
use std::io::BufReader;
use std::path::Path;
use std::process::{Command, Stdio};

use lsp_types::{CompletionList, Position};
use nova_core::{path_to_file_uri, AbsPathBuf};
use tempfile::TempDir;

use crate::support;

fn uri_for_path(path: &Path) -> String {
    let abs = AbsPathBuf::try_from(path.to_path_buf()).expect("abs path");
    path_to_file_uri(&abs).expect("file uri")
}

#[test]
fn stdio_server_hides_ai_code_edit_actions_for_excluded_paths() {
    let _lock = support::stdio_server_lock();

    let temp = TempDir::new().expect("tempdir");
    let secret_dir = temp.path().join("secret");
    fs::create_dir_all(&secret_dir).expect("create secret dir");

    let file_path = secret_dir.join("Test.java");
    let source = "class Test { void run() { } }\n";
    fs::write(&file_path, source).expect("write Test.java");
    let uri = uri_for_path(&file_path);

    // Use a local-only HTTP provider config so the server can enable AI features without
    // requiring any external dependencies. We do not actually execute AI requests in this test.
    let config_path = temp.path().join("nova.toml");
    fs::write(
        &config_path,
        r#"
[ai]
enabled = true

[ai.provider]
kind = "http"
url = "http://127.0.0.1:1/complete"
model = "default"

[ai.privacy]
excluded_paths = ["secret/**"]
"#,
    )
    .expect("write config");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        // The test config file should be authoritative; clear any legacy env-var AI wiring that
        // could override `--config` (common in developer shells).
        .env_remove("NOVA_AI_PROVIDER")
        .env_remove("NOVA_AI_ENDPOINT")
        .env_remove("NOVA_AI_MODEL")
        .env_remove("NOVA_AI_API_KEY")
        .env_remove("NOVA_AI_AUDIT_LOGGING")
        // Ensure a developer's environment doesn't disable AI for this test.
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    // 1) initialize
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

    // 2) open document
    support::write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": uri.clone(),
                    "languageId": "java",
                    "version": 1,
                    "text": source
                }
            }
        }),
    );

    // 3) request code actions with a diagnostic + a non-empty selection. Normally, this would
    // offer AI actions.
    //
    // The file matches `ai.privacy.excluded_paths`, so the server should hide AI *code-editing*
    // actions. Non-edit actions like explain-error remain available (but must omit any excluded
    // code context when building prompts).
    support::write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/codeAction",
            "params": {
                "textDocument": { "uri": uri.clone() },
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 0, "character": 10 }
                },
                "context": {
                    "diagnostics": [{
                        "range": {
                            "start": { "line": 0, "character": 0 },
                            "end": { "line": 0, "character": 10 }
                        },
                        "message": "cannot find symbol"
                    }]
                }
            }
        }),
    );

    let code_actions_resp = support::read_response_with_id(&mut stdout, 2);
    let actions = code_actions_resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("code actions array");

    // Explain-error should remain available, but must not include any source snippet for excluded
    // files.
    let explain = actions
        .iter()
        .find(|a| {
            a.get("command")
                .and_then(|c| c.get("command"))
                .and_then(|v| v.as_str())
                == Some(nova_ide::COMMAND_EXPLAIN_ERROR)
        })
        .expect("expected explain-error action to remain available");

    // Ensure we don't include a code snippet for excluded files.
    let explain_args = explain
        .get("command")
        .and_then(|c| c.get("arguments"))
        .and_then(|v| v.as_array())
        .and_then(|v| v.first())
        .and_then(|v| v.as_object())
        .expect("ExplainErrorArgs payload");
    assert!(
        explain_args.get("code").is_none()
            || explain_args.get("code").is_some_and(|v| v.is_null()),
        "expected explainError args.code to be omitted/null for excluded paths, got: {explain_args:?}"
    );

    // Code-edit actions should be suppressed for excluded paths.
    for cmd in [
        nova_ide::COMMAND_GENERATE_METHOD_BODY,
        nova_ide::COMMAND_GENERATE_TESTS,
    ] {
        assert!(
            actions.iter().all(|a| {
                a.get("command")
                    .and_then(|c| c.get("command"))
                    .and_then(|v| v.as_str())
                    != Some(cmd)
            }),
            "expected AI code edit action {cmd:?} to be suppressed, got: {actions:?}"
        );
    }

    // 4) shutdown + exit
    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 3, "method": "shutdown" }),
    );
    let _shutdown_resp = support::read_response_with_id(&mut stdout, 3);

    support::write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_skips_completion_ranking_for_excluded_paths() {
    let _lock = support::stdio_server_lock();

    let ai_server = support::TestAiServer::start(json!({ "completion": "[0]" }));
    let endpoint = format!("{}/complete", ai_server.base_url());

    let temp = TempDir::new().expect("tempdir");
    let secret_dir = temp.path().join("secret");
    fs::create_dir_all(&secret_dir).expect("create secret dir");

    let excluded_path = secret_dir.join("Test.java");
    let source = concat!(
        "class Test {\n",
        "    void f() {\n",
        "        \n",
        "    }\n",
        "}\n"
    );
    fs::write(&excluded_path, source).expect("write excluded file");
    let excluded_uri = uri_for_path(&excluded_path);

    let allowed_path = temp.path().join("Main.java");
    fs::write(&allowed_path, source).expect("write allowed file");
    let allowed_uri = uri_for_path(&allowed_path);

    let cursor = Position::new(2, source.lines().nth(2).expect("line 2").len() as u32);

    // Enable completion ranking with a local-only HTTP provider.
    let config_path = temp.path().join("nova.toml");
    fs::write(
        &config_path,
        format!(
            r#"
[ai]
enabled = true

[ai.features]
completion_ranking = true

[ai.timeouts]
completion_ranking_ms = 200

[ai.provider]
kind = "http"
url = "{endpoint}"
model = "default"
concurrency = 1

[ai.privacy]
excluded_paths = ["secret/**"]
"#
        ),
    )
    .expect("write config");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        // The test config file should be authoritative; clear any legacy env-var AI wiring that
        // could override `--config` (common in developer shells).
        .env_remove("NOVA_AI_PROVIDER")
        .env_remove("NOVA_AI_ENDPOINT")
        .env_remove("NOVA_AI_MODEL")
        .env_remove("NOVA_AI_API_KEY")
        .env_remove("NOVA_AI_AUDIT_LOGGING")
        // Ensure a developer's environment doesn't disable AI for this test.
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    // 1) initialize
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

    // 2) open documents
    support::write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": excluded_uri.clone(),
                    "languageId": "java",
                    "version": 1,
                    "text": source
                }
            }
        }),
    );
    support::write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": allowed_uri.clone(),
                    "languageId": "java",
                    "version": 1,
                    "text": source
                }
            }
        }),
    );

    // 3) Completion request for excluded path: must not invoke model-backed ranking.
    support::write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/completion",
            "params": {
                "textDocument": { "uri": excluded_uri.clone() },
                "position": cursor,
            }
        }),
    );
    let completion_resp = support::read_response_with_id(&mut stdout, 2);
    let completion_result = completion_resp
        .get("result")
        .cloned()
        .expect("completion result");
    let _list: CompletionList =
        serde_json::from_value(completion_result).expect("decode completion list");
    ai_server.assert_hits(0);

    // 4) Completion request for allowed path should invoke model-backed ranking.
    support::write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "textDocument/completion",
            "params": {
                "textDocument": { "uri": allowed_uri.clone() },
                "position": cursor,
            }
        }),
    );
    let completion_resp = support::read_response_with_id(&mut stdout, 3);
    let completion_result = completion_resp
        .get("result")
        .cloned()
        .expect("completion result");
    let list: CompletionList =
        serde_json::from_value(completion_result).expect("decode completion list");
    assert!(
        list.items.len() > 1,
        "expected completion list to contain multiple items so ranking can run"
    );
    assert!(
        ai_server.hits() > 0,
        "expected non-excluded completion request to invoke model-backed ranking"
    );

    // 5) shutdown + exit
    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 4, "method": "shutdown" }),
    );
    let _shutdown_resp = support::read_response_with_id(&mut stdout, 4);

    support::write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
