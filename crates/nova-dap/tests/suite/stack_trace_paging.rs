use crate::harness::spawn_wire_server;
use nova_jdwp::wire::mock::{MockJdwpServer, MockJdwpServerConfig};
use serde_json::json;

#[tokio::test]
async fn dap_stack_trace_supports_paging_and_total_frames() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();
    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;
    client.attach("127.0.0.1", jdwp.addr().port()).await;

    let thread_id = client.first_thread_id().await;

    // The mock JDWP server adds a new stack frame on the first `StepDepth::Into`.
    client.step_in(thread_id).await;
    let _ = client.wait_for_stopped_reason("step").await;

    let stack = client
        .request(
            "stackTrace",
            json!({
                "threadId": thread_id,
                "startFrame": 1,
                "levels": 1,
            }),
        )
        .await;
    assert_eq!(stack.get("success").and_then(|v| v.as_bool()), Some(true));

    let frames = stack
        .pointer("/body/stackFrames")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("stackTrace response missing body.stackFrames: {stack}"));
    assert_eq!(frames.len(), 1);

    assert_eq!(
        stack.pointer("/body/totalFrames").and_then(|v| v.as_i64()),
        Some(2)
    );

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_stack_trace_clamps_levels_to_available_frames() {
    let jdwp = MockJdwpServer::spawn_with_config(MockJdwpServerConfig {
        thread_frames_strict_length: true,
        ..Default::default()
    })
    .await
    .unwrap();
    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;
    client.attach("127.0.0.1", jdwp.addr().port()).await;

    let thread_id = client.first_thread_id().await;
    client.step_in(thread_id).await;
    let _ = client.wait_for_stopped_reason("step").await;

    // Request "too many" frames; some JVMs reject this with `INVALID_LENGTH`.
    // The adapter should clamp the request based on `ThreadReference.FrameCount`.
    let stack = client
        .request(
            "stackTrace",
            json!({
                "threadId": thread_id,
                "startFrame": 0,
                "levels": 100,
            }),
        )
        .await;
    assert_eq!(stack.get("success").and_then(|v| v.as_bool()), Some(true));

    let frames = stack
        .pointer("/body/stackFrames")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("stackTrace response missing body.stackFrames: {stack}"));
    assert_eq!(frames.len(), 2);
    assert_eq!(
        stack.pointer("/body/totalFrames").and_then(|v| v.as_i64()),
        Some(2)
    );

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_stack_trace_paging_keeps_frame_ids_valid_across_pages() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();
    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;
    client.attach("127.0.0.1", jdwp.addr().port()).await;

    let thread_id = client.first_thread_id().await;
    client.step_in(thread_id).await;
    let _ = client.wait_for_stopped_reason("step").await;

    // Fetch the "bottom" frame via paging.
    let page1 = client
        .request(
            "stackTrace",
            json!({
                "threadId": thread_id,
                "startFrame": 1,
                "levels": 1,
            }),
        )
        .await;
    assert_eq!(page1.get("success").and_then(|v| v.as_bool()), Some(true));
    let page1_frame_id = page1
        .pointer("/body/stackFrames/0/id")
        .and_then(|v| v.as_i64())
        .unwrap_or_else(|| panic!("stackTrace response missing frame id: {page1}"));

    // Frame id should be usable immediately.
    let scopes = client
        .request("scopes", json!({ "frameId": page1_frame_id }))
        .await;
    assert_eq!(scopes.get("success").and_then(|v| v.as_bool()), Some(true));

    // Fetch a different page; this should not invalidate the prior frame id.
    let page0 = client
        .request(
            "stackTrace",
            json!({
                "threadId": thread_id,
                "startFrame": 0,
                "levels": 1,
            }),
        )
        .await;
    assert_eq!(page0.get("success").and_then(|v| v.as_bool()), Some(true));

    let scopes_after = client
        .request("scopes", json!({ "frameId": page1_frame_id }))
        .await;
    assert_eq!(scopes_after.get("success").and_then(|v| v.as_bool()), Some(true));

    // Re-request the same page; the frame id should be stable.
    let page1b = client
        .request(
            "stackTrace",
            json!({
                "threadId": thread_id,
                "startFrame": 1,
                "levels": 1,
            }),
        )
        .await;
    assert_eq!(page1b.get("success").and_then(|v| v.as_bool()), Some(true));
    let page1b_frame_id = page1b
        .pointer("/body/stackFrames/0/id")
        .and_then(|v| v.as_i64())
        .unwrap_or_else(|| panic!("stackTrace response missing frame id: {page1b}"));
    assert_eq!(page1_frame_id, page1b_frame_id);

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}
