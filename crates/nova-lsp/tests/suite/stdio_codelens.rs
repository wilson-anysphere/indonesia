use pretty_assertions::assert_eq;
use serde_json::json;
use std::fs;
use std::io::BufReader;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use tempfile::TempDir;

use crate::support::{file_uri_string, read_response_with_id, write_jsonrpc_message};

#[test]
fn stdio_server_provides_run_test_codelens_for_junit_method() {
    let _lock = crate::support::stdio_server_lock();
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    let file_path = root.join("src/test/java/com/example/CalculatorTest.java");
    fs::create_dir_all(file_path.parent().expect("parent")).expect("mkdir");

    let source = r#"
package com.example;

import org.junit.jupiter.api.Test;

public class CalculatorTest {
    @Test
    void adds() {}
}
"#;
    fs::write(&file_path, source).expect("write test file");

    let uri = file_uri_string(&file_path);
    let root_uri = file_uri_string(root);

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
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
            "params": {
                "rootUri": root_uri,
                "capabilities": {}
            }
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
                    "uri": uri,
                    "languageId": "java",
                    "version": 1,
                    "text": source
                }
            }
        }),
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/codeLens",
            "params": { "textDocument": { "uri": uri } }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let lenses = resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("codeLens result array");
    assert!(
        lenses.iter().any(|lens| {
            lens.pointer("/command/title").and_then(|v| v.as_str()) == Some("Run Test")
        }),
        "expected a Run Test code lens, got: {lenses:?}"
    );

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

#[test]
fn stdio_server_provides_run_main_codelens_for_main_method() {
    let _lock = crate::support::stdio_server_lock();
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    let file_path = root.join("src/main/java/com/example/Main.java");
    fs::create_dir_all(file_path.parent().expect("parent")).expect("mkdir");

    let source = r#"
package com.example;

public class Main {
    public static void main(String[] args) {}
}
"#;
    fs::write(&file_path, source).expect("write main file");

    let uri = file_uri_string(&file_path);
    let root_uri = file_uri_string(root);

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
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
            "params": {
                "rootUri": root_uri,
                "capabilities": {}
            }
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
                    "uri": uri,
                    "languageId": "java",
                    "version": 1,
                    "text": source
                }
            }
        }),
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/codeLens",
            "params": { "textDocument": { "uri": uri } }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let lenses = resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("codeLens result array");
    assert!(
        lenses.iter().any(|lens| {
            lens.pointer("/command/title").and_then(|v| v.as_str()) == Some("Run Main")
        }),
        "expected a Run Main code lens, got: {lenses:?}"
    );

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

#[test]
fn stdio_server_execute_command_runs_tests_via_nova_testing_endpoint() {
    let _lock = crate::support::stdio_server_lock();
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();

    // Minimal Maven marker so nova-testing detects the build tool.
    fs::write(root.join("pom.xml"), "<project/>").expect("write pom");

    // Fake Maven wrapper that writes a Surefire report and exits successfully.
    let mvnw_path = root.join("mvnw");
    fs::write(
        &mvnw_path,
        r#"#!/usr/bin/env sh
set -eu
mkdir -p target/surefire-reports
cat > target/surefire-reports/TEST-com.example.CalculatorTest.xml <<'XML'
<testsuite name="com.example.CalculatorTest" tests="1" failures="0" errors="0" skipped="0">
  <testcase classname="com.example.CalculatorTest" name="adds" time="0.001"/>
</testsuite>
XML
exit 0
"#,
    )
    .expect("write mvnw");
    #[cfg(unix)]
    {
        let mut perms = fs::metadata(&mvnw_path).expect("metadata").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&mvnw_path, perms).expect("chmod mvnw");
    }

    // The test file itself is not used by the runner (we fake Maven), but it keeps the workspace realistic.
    let test_file = root.join("src/test/java/com/example/CalculatorTest.java");
    fs::create_dir_all(test_file.parent().expect("parent")).expect("mkdir");
    fs::write(
        &test_file,
        r#"
package com.example;

import org.junit.jupiter.api.Test;

public class CalculatorTest {
    @Test void adds() {}
}
"#,
    )
    .expect("write test file");

    let root_uri = file_uri_string(root);

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
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
            "params": {
                "rootUri": root_uri,
                "capabilities": {}
            }
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
            "method": "workspace/executeCommand",
            "params": {
                "command": "nova.runTest",
                "arguments": [{ "testId": "com.example.CalculatorTest#adds" }]
            }
        }),
    );

    let resp = read_response_with_id(&mut stdout, 2);
    let result = resp.get("result").expect("executeCommand result");
    assert_eq!(result.get("ok").and_then(|v| v.as_bool()), Some(true));
    assert_eq!(
        result.pointer("/result/success").and_then(|v| v.as_bool()),
        Some(true)
    );

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

#[test]
fn stdio_server_does_not_echo_string_params_in_jsonrpc_errors() {
    let _lock = crate::support::stdio_server_lock();
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    let root_uri = file_uri_string(root);

    let secret = "nova-lsp-secret-token";

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
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
            "params": {
                "rootUri": root_uri,
                "capabilities": {}
            }
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
            "method": "textDocument/codeLens",
            "params": secret,
        }),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let message = resp
        .pointer("/error/message")
        .and_then(|value| value.as_str())
        .expect("error message");

    assert!(
        !message.contains(secret),
        "expected error message to omit secret string values: {message}"
    );
    assert!(
        message.contains("<redacted>"),
        "expected error message to include redaction marker: {message}"
    );

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
