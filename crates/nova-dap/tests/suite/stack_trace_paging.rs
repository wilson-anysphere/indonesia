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
