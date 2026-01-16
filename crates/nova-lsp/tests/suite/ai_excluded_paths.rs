use lsp_types::{CodeActionContext, CodeActionParams, Diagnostic, Position, Range};
use std::fs;
use std::io::BufReader;
use std::path::Path;
use std::process::{Command, Stdio};

use nova_core::{path_to_file_uri, AbsPathBuf};
use tempfile::TempDir;

use crate::support::{
    did_open_notification, exit_notification, initialize_request_empty, initialized_notification,
    jsonrpc_request, read_response_with_id, shutdown_request, stdio_server_lock,
    write_jsonrpc_message,
};

fn uri_for_path(path: &Path) -> String {
    let abs = AbsPathBuf::try_from(path.to_path_buf()).expect("abs path");
    path_to_file_uri(&abs).expect("file uri")
}

#[test]
fn stdio_server_hides_ai_code_edit_actions_for_excluded_paths() {
    let _lock = stdio_server_lock();

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
    write_jsonrpc_message(&mut stdin, &initialize_request_empty(1));
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &initialized_notification());

    // 2) open document
    write_jsonrpc_message(
        &mut stdin,
        &did_open_notification(uri.clone(), "java", 1, source),
    );

    // 3) request code actions with a diagnostic + a non-empty selection. Normally, this would
    // offer AI actions.
    //
    // The file matches `ai.privacy.excluded_paths`, so the server should hide AI *code-editing*
    // actions. Non-edit actions like explain-error remain available (but must omit any excluded
    // code context when building prompts).

    let range = Range {
        start: Position::new(0, 0),
        end: Position::new(0, 10),
    };

    let uri = uri
        .parse()
        .expect("test file URI must parse as lsp_types::Uri");
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            CodeActionParams {
                text_document: lsp_types::TextDocumentIdentifier { uri },
                range,
                context: CodeActionContext {
                    diagnostics: vec![Diagnostic::new_simple(
                        range,
                        "cannot find symbol".to_string(),
                    )],
                    ..CodeActionContext::default()
                },
                work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                partial_result_params: lsp_types::PartialResultParams::default(),
            },
            2,
            "textDocument/codeAction",
        ),
    );

    let code_actions_resp = read_response_with_id(&mut stdout, 2);
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
    write_jsonrpc_message(&mut stdin, &shutdown_request(3));
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);

    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
