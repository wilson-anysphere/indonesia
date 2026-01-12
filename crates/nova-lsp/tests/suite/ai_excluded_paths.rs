use serde_json::json;
use std::fs;
use std::io::BufReader;
use std::path::Path;
use std::process::{Command, Stdio};

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
