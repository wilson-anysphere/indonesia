use crate::harness::spawn_wire_server;
use serde_json::json;

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

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn wire_stream_debug_allows_arrays_stream_expressions() {
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

    let source_elements: Vec<_> = resp
        .pointer("/body/runtime/sourceSample/elements")
        .and_then(|v| v.as_array())
        .expect("missing runtime.sourceSample.elements")
        .iter()
        .map(|v| v.as_str().expect("expected string element"))
        .collect();
    // The wire stream-debug runtime is implemented via compile+inject evaluation. The mock JDWP
    // server currently echoes the maxSampleSize argument back as the return value for the
    // placeholder invocation.
    assert_eq!(source_elements, vec!["3"]);

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}
