use serde_json::json;
use std::io::BufReader;
use std::process::{Command, Stdio};
use tempfile::TempDir;

use crate::support::{read_response_with_id, stdio_server_lock, write_jsonrpc_message};

#[test]
fn stdio_custom_request_invalid_params_does_not_echo_backticked_values() {
    let _lock = stdio_server_lock();

    let secret_suffix = "NOVA_LSP_BACKTICK_SECRET_DO_NOT_LEAK";
    let secret = format!("prefix`, expected {secret_suffix}");

    // Ensure this test would actually catch leaks if sanitization regressed.
    #[derive(Debug, serde::Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct TestSerdeJsonArgs {
        #[allow(dead_code)]
        foo: u32,
        #[allow(dead_code)]
        flag: bool,
    }
    let raw_err = serde_json::from_value::<TestSerdeJsonArgs>(json!({
        "foo": 1,
        "flag": true,
        secret.clone(): 1,
    }))
    .expect_err("expected unknown field error");
    let raw_message = raw_err.to_string();
    assert!(
        raw_message.contains(secret_suffix),
        "expected raw serde_json error string to include the backticked value so this test catches leaks: {raw_message}"
    );

    let temp = TempDir::new().expect("tempdir");
    let config_path = temp.path().join("nova.toml");
    std::fs::write(&config_path, "[ai]\nenabled = false\n").expect("write config");

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
        // Ensure a developer's environment doesn't disable AI unexpectedly (tests that *do* want
        // these overrides set them explicitly).
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
            "params": { "capabilities": {} }
        }),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    let mut params = serde_json::Map::new();
    params.insert("foo".to_string(), json!(1));
    params.insert("flag".to_string(), json!(true));
    params.insert(secret, json!(1));

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/testSerdeJsonArgs",
            "params": serde_json::Value::Object(params),
        }),
    );

    let resp = read_response_with_id(&mut stdout, 2);
    let error = resp.get("error").cloned().expect("expected error response");
    assert_eq!(error.get("code").and_then(|v| v.as_i64()), Some(-32602));
    assert!(
        !resp.to_string().contains(secret_suffix),
        "expected JSON-RPC error to omit backticked values; got: {resp:?}"
    );
    assert!(
        resp.to_string().contains("<redacted>"),
        "expected JSON-RPC error to include redaction marker; got: {resp:?}"
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 99, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 99);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_custom_request_invalid_params_does_not_echo_backticked_numeric_values() {
    let _lock = stdio_server_lock();

    let secret_number = 9_876_543_210u64;
    let secret_text = secret_number.to_string();

    // Ensure this test would actually catch leaks if sanitization regressed.
    #[derive(Debug, serde::Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct TestSerdeJsonArgs {
        #[allow(dead_code)]
        foo: u32,
        #[allow(dead_code)]
        flag: bool,
    }
    let raw_err = serde_json::from_value::<TestSerdeJsonArgs>(json!({
        "foo": 1,
        "flag": secret_number,
    }))
    .expect_err("expected invalid type error");
    let raw_message = raw_err.to_string();
    assert!(
        raw_message.contains(&secret_text),
        "expected raw serde_json error string to include the backticked numeric value so this test catches leaks: {raw_message}"
    );

    let temp = TempDir::new().expect("tempdir");
    let config_path = temp.path().join("nova.toml");
    std::fs::write(&config_path, "[ai]\nenabled = false\n").expect("write config");

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
        // Ensure a developer's environment doesn't disable AI unexpectedly (tests that *do* want
        // these overrides set them explicitly).
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
            "params": { "capabilities": {} }
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
            "id": 2,
            "method": "nova/testSerdeJsonArgs",
            // `flag` expects a boolean. Sending an integer triggers:
            // `invalid type: integer `...`, expected a boolean`.
            "params": { "foo": 1, "flag": secret_number },
        }),
    );

    let resp = read_response_with_id(&mut stdout, 2);
    let error = resp.get("error").cloned().expect("expected error response");
    assert_eq!(error.get("code").and_then(|v| v.as_i64()), Some(-32602));
    assert!(
        !resp.to_string().contains(&secret_text),
        "expected JSON-RPC error to omit backticked numeric values; got: {resp:?}"
    );
    assert!(
        resp.to_string().contains("<redacted>"),
        "expected JSON-RPC error to include redaction marker; got: {resp:?}"
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 99, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 99);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
