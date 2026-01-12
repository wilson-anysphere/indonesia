mod harness;

use std::time::Duration;

use harness::spawn_wire_server;
use nova_jdwp::wire::mock::{DelayedReply, MockJdwpServerConfig};
use serde_json::json;

#[tokio::test]
async fn stream_debug_does_not_deadlock_event_task_and_cancels_cleanly() {
    let (client, server_task) = spawn_wire_server();
    client.initialize_handshake().await;

    // Delay the JDWP reply used by stream-debug evaluation (InvokeMethod), then emit a breakpoint
    // event while the reply is pending. Historically, holding the debugger mutex while awaiting
    // InvokeMethod could deadlock the event forwarding task.
    let _jdwp = client
        .attach_mock_jdwp_with_config(MockJdwpServerConfig {
            delayed_replies: vec![DelayedReply {
                command_set: 3,
                command: 3, // ClassType.InvokeMethod
                delay: Duration::from_secs(5),
            }],
            // Need one breakpoint event to stop on `continue`, and one more to fire during
            // InvokeMethod.
            breakpoint_events: 2,
            ..Default::default()
        })
        .await;

    let bp_resp = client.set_breakpoints("Main.java", &[3]).await;
    assert_eq!(
        bp_resp
            .pointer("/body/breakpoints/0/verified")
            .and_then(|v| v.as_bool()),
        Some(true),
        "expected breakpoint to be installed: {bp_resp}"
    );

    client.continue_().await;
    let stopped = client.wait_for_stopped_reason("breakpoint").await;
    let thread_id = stopped
        .thread_id
        .unwrap_or_else(|| panic!("stopped event missing threadId: {}", stopped.raw));

    let frame_id = client.first_frame_id(thread_id).await;

    let stream_seq = client
        .send_request(
            "nova/streamDebug",
            json!({
                "expression": "list.stream().count()",
                "frameId": frame_id,
                "maxSampleSize": 2,
                "maxTotalTimeMs": 10_000,
                "allowSideEffects": false,
                "allowTerminalOps": true,
            }),
        )
        .await;

    // Ensure the adapter can still process events while stream-debug is awaiting InvokeMethod.
    // If the request handler holds the debugger lock, this will time out.
    let second_stop = client
        .wait_for_event_matching(
            "stopped during stream debug InvokeMethod",
            Duration::from_secs(2),
            |msg| {
                msg.get("type").and_then(|v| v.as_str()) == Some("event")
                    && msg.get("event").and_then(|v| v.as_str()) == Some("stopped")
            },
        )
        .await;
    assert_eq!(
        second_stop.pointer("/body/reason").and_then(|v| v.as_str()),
        Some("breakpoint"),
        "unexpected stopped event while stream debug in-flight: {second_stop}"
    );

    let cancel_seq = client
        .send_request("cancel", json!({ "requestId": stream_seq }))
        .await;

    let cancel_resp = client.wait_for_response(cancel_seq).await;
    assert!(
        cancel_resp
            .get("success")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        "cancel request was not successful: {cancel_resp}"
    );

    let stream_resp = client.wait_for_response(stream_seq).await;
    assert!(
        !stream_resp
            .get("success")
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
        "expected stream debug response to be cancelled: {stream_resp}"
    );
    assert_eq!(
        stream_resp.get("message").and_then(|v| v.as_str()),
        Some("cancelled"),
        "expected cancelled message: {stream_resp}"
    );

    // Verify the adapter remains responsive after cancellation.
    let threads = client.request("threads", json!({})).await;
    assert!(
        threads
            .get("success")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        "threads request failed after cancellation: {threads}"
    );

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

