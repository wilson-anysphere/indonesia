use crate::harness::spawn_wire_server;
use serde_json::json;

#[tokio::test]
async fn wire_server_does_not_echo_string_values_in_launch_argument_errors() {
    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;

    let secret_suffix = "nova-dap-wire-super-secret-token";
    let secret = format!("prefix\"{secret_suffix}");
    // `launch.port` expects a number (`u16`). Passing a string triggers:
    // `invalid type: string "..."`.
    let resp = client.request("launch", json!({ "port": secret })).await;

    assert_eq!(
        resp.get("success").and_then(|v| v.as_bool()),
        Some(false),
        "unexpected response: {resp}"
    );

    let message = resp
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(
        !message.contains(secret_suffix),
        "expected DAP response error message to omit string values: {message}"
    );
    assert!(
        message.contains("<redacted>"),
        "expected DAP response error message to include redaction marker: {message}"
    );

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn wire_server_does_not_echo_backticked_numeric_values_in_launch_argument_errors() {
    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;

    let secret_number = 9_876_543_210u64;
    let secret_text = secret_number.to_string();
    // `launch.port` expects a number (`u16`). Passing an out-of-range integer triggers:
    // `invalid value: integer `...`, expected u16`.
    let resp = client
        .request("launch", json!({ "port": secret_number }))
        .await;

    assert_eq!(
        resp.get("success").and_then(|v| v.as_bool()),
        Some(false),
        "unexpected response: {resp}"
    );

    let message = resp
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(
        !message.contains(&secret_text),
        "expected DAP response error message to omit numeric values: {message}"
    );
    assert!(
        message.contains("<redacted>"),
        "expected DAP response error message to include redaction marker: {message}"
    );

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn wire_server_does_not_echo_backticked_values_in_test_argument_errors() {
    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;

    let secret_suffix = "nova-dap-wire-backticked-secret";
    let secret = format!("prefix`, expected {secret_suffix}");

    let mut args = serde_json::Map::new();
    args.insert(secret, json!(1));
    let resp = client
        .request("nova/testSerdeJsonArgs", serde_json::Value::Object(args))
        .await;

    assert_eq!(
        resp.get("success").and_then(|v| v.as_bool()),
        Some(false),
        "unexpected response: {resp}"
    );

    let message = resp
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(
        !message.contains(secret_suffix),
        "expected DAP response error message to omit backticked values: {message}"
    );
    assert!(
        message.contains("<redacted>"),
        "expected DAP response error message to include redaction marker: {message}"
    );

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}
