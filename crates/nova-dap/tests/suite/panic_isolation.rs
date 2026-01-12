use crate::harness::spawn_wire_server;
use serde_json::json;

#[tokio::test]
async fn request_handler_panic_isolation_sends_error_response_and_continues() {
    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;

    let resp = client.request("nova/testPanic", json!({})).await;
    assert_eq!(
        resp.get("success").and_then(|v| v.as_bool()),
        Some(false),
        "expected error response for panic request: {resp}"
    );
    let msg = resp.get("message").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        msg.contains("internal error") && msg.contains("panic"),
        "expected panic message, got: {resp}"
    );

    // The server should remain alive and able to handle subsequent requests.
    let resp2 = client.request("initialize", json!({})).await;
    assert_eq!(
        resp2.get("success").and_then(|v| v.as_bool()),
        Some(true),
        "server did not respond successfully after panic: {resp2}"
    );

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

