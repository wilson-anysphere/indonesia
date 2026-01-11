mod harness;

use harness::{spawn_wire_server, transcript as tr};
use nova_jdwp::wire::mock::{DelayedReply, MockJdwpServer, MockJdwpServerConfig};
use serde_json::json;

#[tokio::test]
async fn transcript_attach_breakpoints_continue_stop_disconnect() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();
    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;
    client.attach("127.0.0.1", jdwp.addr().port()).await;
    client.set_breakpoints("Main.java", &[3]).await;
    client.continue_().await;
    let _ = client.wait_for_stopped_reason("breakpoint").await;
    client.disconnect().await;

    server_task.await.unwrap().unwrap();

    let expected = vec![
        tr::request("initialize", json!({})),
        tr::response(
            "initialize",
            true,
            Some(json!({
                "supportsConfigurationDoneRequest": true,
                "supportsEvaluateForHovers": true,
                 "supportsPauseRequest": true,
                 "supportsCancelRequest": true,
                 "supportsSetVariable": false,
                 "supportsStepBack": false,
                 "supportsExceptionBreakpoints": true,
                 "supportsExceptionInfoRequest": true,
                 "supportsConditionalBreakpoints": true,
              })),
         ),
        tr::request(
            "attach",
            json!({
                "host": "127.0.0.1",
                "port": tr::ignore(),
            }),
        ),
        tr::response("attach", true, None),
        tr::event("initialized", None),
        tr::request(
            "setBreakpoints",
            json!({
                "source": { "path": "Main.java" },
                "breakpoints": [ { "line": 3 } ],
            }),
        ),
        tr::response(
            "setBreakpoints",
            true,
            Some(json!({
                "breakpoints": [ { "verified": true, "line": 3 } ],
            })),
        ),
        tr::request("continue", json!({})),
        tr::response(
            "continue",
            true,
            Some(json!({
                "allThreadsContinued": true,
            })),
        ),
        tr::event(
            "continued",
            Some(json!({
                "allThreadsContinued": true,
            })),
        ),
        tr::event(
            "stopped",
            Some(json!({
                "reason": "breakpoint",
                "threadId": tr::ignore(),
                "allThreadsStopped": false,
            })),
        ),
        tr::request("disconnect", json!({})),
        tr::response("disconnect", true, None),
        tr::event("terminated", None),
    ];

    client.assert_transcript(&expected).await;
}

#[tokio::test]
async fn transcript_step_sequences() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();
    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;
    client.attach("127.0.0.1", jdwp.addr().port()).await;
    client.set_breakpoints("Main.java", &[3]).await;

    client.continue_().await;
    let stopped = client.wait_for_stopped_reason("breakpoint").await;
    let thread_id = stopped.thread_id.expect("stopped event missing threadId");

    client.next(thread_id).await;
    let _ = client.wait_for_stopped_reason("step").await;

    client.step_in(thread_id).await;
    let _ = client.wait_for_stopped_reason("step").await;

    client.step_out(thread_id).await;
    let _ = client.wait_for_stopped_reason("step").await;

    client.disconnect().await;

    server_task.await.unwrap().unwrap();

    let expected = vec![
        tr::request("initialize", json!({})),
        tr::response(
            "initialize",
            true,
            Some(json!({
                "supportsConfigurationDoneRequest": true,
                "supportsEvaluateForHovers": true,
                 "supportsPauseRequest": true,
                 "supportsCancelRequest": true,
                 "supportsSetVariable": false,
                 "supportsStepBack": false,
                 "supportsExceptionBreakpoints": true,
                 "supportsExceptionInfoRequest": true,
                 "supportsConditionalBreakpoints": true,
              })),
         ),
        tr::request(
            "attach",
            json!({
                "host": "127.0.0.1",
                "port": tr::ignore(),
            }),
        ),
        tr::response("attach", true, None),
        tr::event("initialized", None),
        tr::request(
            "setBreakpoints",
            json!({
                "source": { "path": "Main.java" },
                "breakpoints": [ { "line": 3 } ],
            }),
        ),
        tr::response(
            "setBreakpoints",
            true,
            Some(json!({
                "breakpoints": [ { "verified": true, "line": 3 } ],
            })),
        ),
        tr::request("continue", json!({})),
        tr::response(
            "continue",
            true,
            Some(json!({
                "allThreadsContinued": true,
            })),
        ),
        tr::event(
            "continued",
            Some(json!({
                "allThreadsContinued": true,
            })),
        ),
        tr::event(
            "stopped",
            Some(json!({
                "reason": "breakpoint",
                "threadId": tr::ignore(),
                "allThreadsStopped": false,
            })),
        ),
        tr::request("next", json!({ "threadId": tr::ignore() })),
        tr::response("next", true, None),
        tr::event(
            "stopped",
            Some(json!({
                "reason": "step",
                "threadId": tr::ignore(),
                "allThreadsStopped": false,
            })),
        ),
        tr::request("stepIn", json!({ "threadId": tr::ignore() })),
        tr::response("stepIn", true, None),
        tr::event(
            "stopped",
            Some(json!({
                "reason": "step",
                "threadId": tr::ignore(),
                "allThreadsStopped": false,
            })),
        ),
        tr::request("stepOut", json!({ "threadId": tr::ignore() })),
        tr::response("stepOut", true, None),
        tr::event(
            "stopped",
            Some(json!({
                "reason": "step",
                "threadId": tr::ignore(),
                "allThreadsStopped": false,
            })),
        ),
        tr::request("disconnect", json!({})),
        tr::response("disconnect", true, None),
        tr::event("terminated", None),
    ];

    client.assert_transcript(&expected).await;
}

#[tokio::test]
async fn transcript_cancel_delayed_request() {
    let mut cfg = MockJdwpServerConfig::default();
    // Artificially delay the JDWP call used by `threads` so we have time to send `cancel`.
    cfg.delayed_replies.push(DelayedReply {
        command_set: 1,
        command: 4,
        delay: std::time::Duration::from_secs(5),
    });
    let jdwp = MockJdwpServer::spawn_with_config(cfg).await.unwrap();

    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;
    client.attach("127.0.0.1", jdwp.addr().port()).await;

    // Send a slow request without awaiting it, then cancel it.
    let threads_seq = client.send_request("threads", json!({})).await;
    let cancel_seq = client
        .send_request(
            "cancel",
            json!({
                "requestId": threads_seq,
            }),
        )
        .await;
    let cancel_resp = client.wait_for_response_with_timeout(cancel_seq, std::time::Duration::from_secs(10)).await;
    assert_eq!(
        cancel_resp.get("success").and_then(|v| v.as_bool()),
        Some(true),
        "expected cancel to succeed: {cancel_resp}",
    );

    let threads_resp = client.wait_for_response_with_timeout(threads_seq, std::time::Duration::from_secs(10)).await;
    assert_eq!(
        threads_resp.get("success").and_then(|v| v.as_bool()),
        Some(false),
        "expected threads to be cancelled: {threads_resp}",
    );
    assert_eq!(
        threads_resp.get("message").and_then(|v| v.as_str()),
        Some("cancelled"),
        "expected threads cancellation message: {threads_resp}",
    );

    client.disconnect().await;

    server_task.await.unwrap().unwrap();

    let expected = vec![
        tr::request("initialize", json!({})),
        tr::response(
            "initialize",
            true,
            Some(json!({
                "supportsConfigurationDoneRequest": true,
                "supportsEvaluateForHovers": true,
                 "supportsPauseRequest": true,
                 "supportsCancelRequest": true,
                 "supportsSetVariable": false,
                 "supportsStepBack": false,
                 "supportsExceptionBreakpoints": true,
                 "supportsExceptionInfoRequest": true,
                 "supportsConditionalBreakpoints": true,
              })),
         ),
        tr::request(
            "attach",
            json!({
                "host": "127.0.0.1",
                "port": tr::ignore(),
            }),
        ),
        tr::response("attach", true, None),
        tr::event("initialized", None),
        tr::request("threads", json!({})),
        tr::request("cancel", json!({ "requestId": tr::ignore() })),
        tr::response("cancel", true, None),
        tr::response(
            "threads",
            false,
            None,
        ),
        tr::request("disconnect", json!({})),
        tr::response("disconnect", true, None),
        tr::event("terminated", None),
    ];

    client.assert_transcript(&expected).await;
}

#[tokio::test]
#[ignore = "Optional stress/regression test (Task 208)"]
async fn stress_handles_do_not_grow_unbounded() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();
    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;
    client.attach("127.0.0.1", jdwp.addr().port()).await;
    client.set_breakpoints("Main.java", &[3]).await;
    client.continue_().await;

    let stopped = client.wait_for_stopped_reason("breakpoint").await;
    let thread_id = stopped.thread_id.expect("stopped event missing threadId");

    let mut max_frame_id = 0i64;
    let mut max_scope_ref = 0i64;

    for _ in 0..200 {
        let frame_id = client.first_frame_id(thread_id).await;
        max_frame_id = max_frame_id.max(frame_id);

        let locals_ref = client.first_scope_variables_reference(frame_id).await;
        max_scope_ref = max_scope_ref.max(locals_ref);

        let _ = client.variables(locals_ref).await;

        // Move to a fresh stop; if handle tables are reset per-stop, IDs should remain bounded.
        client.next(thread_id).await;
        let _ = client.wait_for_stopped_reason("step").await;
    }

    assert!(
        max_frame_id < 50 && max_scope_ref < 50,
        "handle tables grew unexpectedly: max_frame_id={max_frame_id}, max_scope_ref={max_scope_ref}"
    );

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}
