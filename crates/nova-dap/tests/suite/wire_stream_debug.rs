use std::process::{Command, Stdio};

use crate::harness::spawn_wire_server;
use serde_json::json;

fn tool_available(name: &str) -> bool {
    Command::new(name)
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

#[tokio::test]
async fn wire_stream_debug_requires_attached_session() {
    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;

    let resp = client
        .request(
            "nova/streamDebug",
            json!({
                "expression": "list.stream().count()",
                "frameId": 1,
            }),
        )
        .await;

    assert_eq!(
        resp.get("success").and_then(|v| v.as_bool()),
        Some(false),
        "unexpected response: {resp}"
    );
    assert_eq!(
        resp.get("message").and_then(|v| v.as_str()),
        Some("not attached"),
        "unexpected response: {resp}"
    );

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn wire_stream_debug_requires_frame_id() {
    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;
    let _jdwp = client.attach_mock_jdwp().await;

    let resp = client
        .request(
            "nova/streamDebug",
            json!({
                "expression": "list.stream().count()",
            }),
        )
        .await;

    assert_eq!(
        resp.get("success").and_then(|v| v.as_bool()),
        Some(false),
        "unexpected response: {resp}"
    );
    let msg = resp.get("message").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        msg.contains("frameId"),
        "expected message to mention frameId: {resp}"
    );

    let resp = client
        .request(
            "nova/streamDebug",
            json!({
                "expression": "list.stream().count()",
                "frameId": 0,
            }),
        )
        .await;

    assert_eq!(
        resp.get("success").and_then(|v| v.as_bool()),
        Some(false),
        "unexpected response: {resp}"
    );
    let msg = resp.get("message").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        msg.contains("frameId"),
        "expected message to mention frameId: {resp}"
    );

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn wire_stream_debug_refuses_existing_stream_values() {
    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;
    let _jdwp = client.attach_mock_jdwp().await;

    let thread_id = client.first_thread_id().await;
    let frame_id = client.first_frame_id(thread_id).await;

    let resp = client
        .request(
            "nova/streamDebug",
            json!({
                "expression": "s.filter(x -> x > 0).count()",
                "frameId": frame_id,
                "allowTerminalOps": true,
            }),
        )
        .await;

    assert_eq!(
        resp.get("success").and_then(|v| v.as_bool()),
        Some(false),
        "unexpected response: {resp}"
    );
    let msg = resp.get("message").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        msg.contains("refusing to run stream debug") && msg.contains("existing Stream value"),
        "unexpected message: {msg}"
    );

    // Ensure the debug session remains usable after the guard triggers.
    let threads = client.request("threads", json!({})).await;
    assert_eq!(
        threads.get("success").and_then(|v| v.as_bool()),
        Some(true),
        "unexpected threads response: {threads}"
    );

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn wire_stream_debug_refuses_existing_stream_values_with_parenthesized_pipeline() {
    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;
    let _jdwp = client.attach_mock_jdwp().await;

    let thread_id = client.first_thread_id().await;
    let frame_id = client.first_frame_id(thread_id).await;

    // Parenthesized pipelines should not bypass the existing stream value safety guard.
    let resp = client
        .request(
            "nova/streamDebug",
            json!({
                "expression": "(s.filter(x -> x > 0)).count()",
                "frameId": frame_id,
                "allowTerminalOps": true,
            }),
        )
        .await;

    assert_eq!(
        resp.get("success").and_then(|v| v.as_bool()),
        Some(false),
        "unexpected response: {resp}"
    );
    let msg = resp.get("message").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        msg.contains("refusing to run stream debug") && msg.contains("existing Stream value"),
        "unexpected message: {msg}"
    );

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn wire_stream_debug_refuses_existing_stream_values_with_parens_in_index() {
    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;
    let _jdwp = client.attach_mock_jdwp().await;

    let thread_id = client.first_thread_id().await;
    let frame_id = client.first_frame_id(thread_id).await;

    // `streams[(i)]` is still just an access path to an existing stream value (and therefore
    // unsafe to sample), even though the expression contains parentheses.
    let resp = client
        .request(
            "nova/streamDebug",
            json!({
                "expression": "streams[(i)].filter(x -> x > 0).count()",
                "frameId": frame_id,
                "allowTerminalOps": true,
            }),
        )
        .await;

    assert_eq!(
        resp.get("success").and_then(|v| v.as_bool()),
        Some(false),
        "unexpected response: {resp}"
    );
    let msg = resp.get("message").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        msg.contains("refusing to run stream debug") && msg.contains("existing Stream value"),
        "unexpected message: {msg}"
    );

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn wire_stream_debug_allows_arrays_stream_expressions() {
    if !tool_available("javac") {
        eprintln!("skipping stream debug wire integration test: javac not available");
        return;
    }

    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;
    let _jdwp = client.attach_mock_jdwp().await;

    let thread_id = client.first_thread_id().await;
    let frame_id = client.first_frame_id(thread_id).await;

    let resp = client
        .request(
            "nova/streamDebug",
            json!({
                "expression": "java.util.Arrays.stream(arr).filter(x -> x > 0).count()",
                "frameId": frame_id,
                "maxSampleSize": 3,
                "allowTerminalOps": true,
            }),
        )
        .await;

    assert_eq!(
        resp.get("success").and_then(|v| v.as_bool()),
        Some(true),
        "unexpected response: {resp}"
    );

    // Ensure we did not reject call-based `ExistingStream` sources (the safety guard is meant
    // to block only *existing stream values*, like a `Stream` local variable).
    assert_eq!(
        resp.pointer("/body/analysis/source/kind")
            .and_then(|v| v.as_str()),
        Some("existingStream"),
        "unexpected analysis source: {resp}"
    );
    let stream_expr = resp
        .pointer("/body/analysis/source/stream_expr")
        .or_else(|| resp.pointer("/body/analysis/source/streamExpr"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        stream_expr.contains("Arrays.stream"),
        "expected analysis source stream_expr to include Arrays.stream(...): {resp}"
    );

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn wire_stream_debug_allows_parenthesized_arrays_stream_expressions() {
    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;
    let _jdwp = client.attach_mock_jdwp().await;

    let thread_id = client.first_thread_id().await;
    let frame_id = client.first_frame_id(thread_id).await;

    let resp = client
        .request(
            "nova/streamDebug",
            json!({
                "expression": "(java.util.Arrays.stream(arr)).filter(x -> x > 0).count()",
                "frameId": frame_id,
                "maxSampleSize": 3,
                "allowTerminalOps": true,
            }),
        )
        .await;

    assert_eq!(
        resp.get("success").and_then(|v| v.as_bool()),
        Some(true),
        "unexpected response: {resp}"
    );

    assert_eq!(
        resp.pointer("/body/analysis/source/kind")
            .and_then(|v| v.as_str()),
        Some("existingStream"),
        "unexpected analysis source: {resp}"
    );
    let stream_expr = resp
        .pointer("/body/analysis/source/stream_expr")
        .or_else(|| resp.pointer("/body/analysis/source/streamExpr"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        stream_expr.contains("Arrays.stream"),
        "expected analysis source stream_expr to include Arrays.stream(...): {resp}"
    );

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}
