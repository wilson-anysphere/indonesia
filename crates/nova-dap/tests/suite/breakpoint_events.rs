use crate::harness::spawn_wire_server;
use nova_jdwp::wire::mock::MockJdwpServerConfig;

#[tokio::test]
async fn emits_breakpoint_event_when_class_prepare_verifies_pending_breakpoint() {
    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;
    let _jdwp = client
        .attach_mock_jdwp_with_config(MockJdwpServerConfig {
            all_classes_initially_loaded: false,
            class_prepare_events: 1,
            ..Default::default()
        })
        .await;

    let bp_resp = client.set_breakpoints("Main.java", &[3]).await;
    assert_eq!(
        bp_resp
            .pointer("/body/breakpoints/0/verified")
            .and_then(|v| v.as_bool()),
        Some(false),
        "expected breakpoint to be unverified before class is loaded: {bp_resp}"
    );
    let msg = bp_resp
        .pointer("/body/breakpoints/0/message")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        msg.contains("class not loaded"),
        "expected class-not-loaded message, got {msg:?}"
    );

    client.continue_().await;

    let evt = client.wait_for_event("breakpoint").await;
    assert_eq!(
        evt.pointer("/body/breakpoint/verified")
            .and_then(|v| v.as_bool()),
        Some(true),
        "expected breakpoint event to report verified breakpoint: {evt}"
    );
    assert_eq!(
        evt.pointer("/body/breakpoint/line")
            .and_then(|v| v.as_i64()),
        Some(3),
        "expected breakpoint event to report line 3: {evt}"
    );

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}
