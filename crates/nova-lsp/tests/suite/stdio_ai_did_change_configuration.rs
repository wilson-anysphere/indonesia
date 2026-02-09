use serde_json::json;
use std::fs;
use std::io::BufReader;
use std::process::{Command, Stdio};
use tempfile::TempDir;

use crate::support::{file_uri_string, read_response_with_id, stdio_server_lock, write_jsonrpc_message};

fn find_action<'a>(actions: &'a [serde_json::Value], command: &str) -> Option<&'a serde_json::Value> {
    actions.iter().find(|action| {
        action
            .pointer("/command/command")
            .and_then(|v| v.as_str())
            == Some(command)
    })
}

fn explain_error_code(action: &serde_json::Value) -> Option<&serde_json::Value> {
    action.pointer("/command/arguments/0/code")
}

#[test]
fn stdio_did_change_configuration_reloads_ai_privacy_excluded_paths() {
    let _lock = stdio_server_lock();

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path().canonicalize().expect("canonical root");
    let root_uri = file_uri_string(&root);

    let cache_dir = TempDir::new().expect("cache dir");

    let secret_dir = root.join("src").join("secrets");
    fs::create_dir_all(&secret_dir).expect("create src/secrets");
    let secret_path = secret_dir.join("Secret.java");
    let source = "class Secret { void run() { unknown(); } }\n";
    fs::write(&secret_path, source).expect("write Secret.java");
    let secret_uri = file_uri_string(&secret_path);

    let config_path = root.join("nova.toml");
    fs::write(
        &config_path,
        r#"
[ai]
enabled = true

[ai.provider]
kind = "http"
url = "http://127.0.0.1:1/complete"
model = "default"
concurrency = 1
"#,
    )
    .expect("write config");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        .env("NOVA_CACHE_DIR", cache_dir.path())
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
        .env_remove("NOVA_AI_COMPLETIONS_MAX_ITEMS")
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
            "params": { "rootUri": root_uri, "capabilities": {} }
        }),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
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
                    "uri": secret_uri.clone(),
                    "languageId": "java",
                    "version": 1,
                    "text": source
                }
            }
        }),
    );

    let code_action_params = json!({
        "textDocument": { "uri": secret_uri.clone() },
        "range": {
            "start": { "line": 0, "character": 0 },
            "end": { "line": 0, "character": 20 }
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
    });

    // 1) With no excluded_paths, AI actions should be offered for the Secret.java file.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/codeAction",
            "params": code_action_params.clone()
        }),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let actions = resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("code actions array");

    let explain = find_action(actions, nova_ide::COMMAND_EXPLAIN_ERROR)
        .expect("expected Explain this error code action when AI is configured");
    assert!(
        explain_error_code(explain).is_some_and(|v| v.is_string()),
        "expected ExplainErrorArgs.code to be present before excluded_paths is configured; got: {explain:?}"
    );
    assert!(
        find_action(actions, nova_ide::COMMAND_GENERATE_TESTS).is_some(),
        "expected Generate tests action before excluded_paths is configured; got: {actions:?}"
    );

    // 2) Update config on disk and notify the server via `workspace/didChangeConfiguration`.
    fs::write(
        &config_path,
        r#"
[ai]
enabled = true

[ai.provider]
kind = "http"
url = "http://127.0.0.1:1/complete"
model = "default"
concurrency = 1

[ai.privacy]
excluded_paths = ["src/secrets/**"]
"#,
    )
    .expect("rewrite config");

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "workspace/didChangeConfiguration",
            "params": { "settings": {} }
        }),
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "textDocument/codeAction",
            "params": code_action_params.clone()
        }),
    );
    let resp = read_response_with_id(&mut stdout, 3);
    let actions = resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("code actions array");

    let explain = find_action(actions, nova_ide::COMMAND_EXPLAIN_ERROR)
        .expect("expected Explain this error code action after excluded_paths update");
    let explain_code = explain_error_code(explain).expect("ExplainErrorArgs.code field");
    assert!(
        explain_code.is_null(),
        "expected ExplainErrorArgs.code to be null after excluded_paths update; got: {explain_code:?}"
    );
    assert!(
        find_action(actions, nova_ide::COMMAND_GENERATE_TESTS).is_none(),
        "expected Generate tests action to be suppressed after excluded_paths update; got: {actions:?}"
    );

    // 3) Removing excluded_paths should re-enable AI code-edit actions for the file.
    fs::write(
        &config_path,
        r#"
[ai]
enabled = true

[ai.provider]
kind = "http"
url = "http://127.0.0.1:1/complete"
model = "default"
concurrency = 1
"#,
    )
    .expect("rewrite config (remove excluded_paths)");
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "workspace/didChangeConfiguration",
            "params": { "settings": {} }
        }),
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "textDocument/codeAction",
            "params": code_action_params
        }),
    );
    let resp = read_response_with_id(&mut stdout, 4);
    let actions = resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("code actions array");
    let explain = find_action(actions, nova_ide::COMMAND_EXPLAIN_ERROR)
        .expect("expected Explain this error code action after excluded_paths removal");
    assert!(
        explain_error_code(explain).is_some_and(|v| v.is_string()),
        "expected ExplainErrorArgs.code to be present after excluded_paths removal; got: {explain:?}"
    );
    assert!(
        find_action(actions, nova_ide::COMMAND_GENERATE_TESTS).is_some(),
        "expected Generate tests action after excluded_paths removal; got: {actions:?}"
    );

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
