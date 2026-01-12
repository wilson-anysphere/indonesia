use crate::harness::spawn_wire_server;
use nova_jdwp::wire::mock::MockJdwpServer;

#[tokio::test]
async fn set_breakpoints_before_attach_are_cached_and_applied_on_attach() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();
    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;

    // Breakpoint configuration is allowed before the debugger is attached. The adapter should
    // acknowledge the request, but mark breakpoints as pending/unverified until attach/launch
    // completes.
    let bp_resp = client.set_breakpoints("Main.java", &[3]).await;
    assert_eq!(
        bp_resp
            .pointer("/body/breakpoints/0/verified")
            .and_then(|v| v.as_bool()),
        Some(false),
        "expected breakpoint to be unverified before attach: {bp_resp}"
    );

    let msg = bp_resp
        .pointer("/body/breakpoints/0/message")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        msg.contains("pending"),
        "expected pending message before attach, got {msg:?}"
    );

    client.attach("127.0.0.1", jdwp.addr().port()).await;

    // Cached breakpoints should be installed automatically during attach.
    assert_eq!(jdwp.breakpoint_suspend_policy().await, Some(1));

    client.continue_().await;
    let _ = client.wait_for_stopped_reason("breakpoint").await;

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}
