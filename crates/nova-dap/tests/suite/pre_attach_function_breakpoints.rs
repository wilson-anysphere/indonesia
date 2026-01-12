use crate::harness::spawn_wire_server;
use nova_jdwp::wire::mock::MockJdwpServer;
use serde_json::json;

#[tokio::test]
async fn set_function_breakpoints_before_attach_are_cached_and_applied_on_attach() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();
    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;

    let resp = client
        .request(
            "setFunctionBreakpoints",
            json!({
                "breakpoints": [{ "name": "Main.main" }],
            }),
        )
        .await;
    assert_eq!(
        resp.get("success").and_then(|v| v.as_bool()),
        Some(true),
        "expected setFunctionBreakpoints to succeed before attach: {resp}"
    );
    assert_eq!(
        resp.pointer("/body/breakpoints/0/verified")
            .and_then(|v| v.as_bool()),
        Some(false),
        "expected function breakpoint to be unverified before attach: {resp}"
    );

    let msg = resp
        .pointer("/body/breakpoints/0/message")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        msg.contains("pending"),
        "expected pending message before attach, got {msg:?}"
    );

    client.attach("127.0.0.1", jdwp.addr().port()).await;

    client.continue_().await;
    let _ = client.wait_for_stopped_reason("breakpoint").await;

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}
