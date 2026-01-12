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
