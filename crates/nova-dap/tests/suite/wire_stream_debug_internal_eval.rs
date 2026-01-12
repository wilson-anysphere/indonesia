use std::{
    process::{Command, Stdio},
    time::Duration,
};

use crate::harness::spawn_wire_server;
use nova_jdwp::wire::mock::MockJdwpServerConfig;
use serde_json::json;

fn tool_available(name: &str) -> bool {
    Command::new(name)
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

#[tokio::test]
async fn stream_debug_internal_eval_does_not_stop_or_mutate_hit_counts() {
    if !tool_available("javac") {
        eprintln!("skipping stream debug internal evaluation regression test: javac not available");
        return;
    }

    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;

    // Configure the mock to:
    // - emit a breakpoint stop event *during* InvokeMethod
    // - delay the invoke reply until the adapter resumes the thread
    // - only emit one breakpoint stop event after each resume (so hit-count breakpoints won't
    //   eventually stop unless the hit count was mutated by internal evaluation).
    let jdwp = client
        .attach_mock_jdwp_with_config(MockJdwpServerConfig {
            breakpoint_events: 1,
            invoke_method_breakpoint_events: 1,
            ..Default::default()
        })
        .await;

    // A hit-count breakpoint that stops only on the second hit.
    let bp_resp = client
        .request(
            "setBreakpoints",
            json!({
                "source": { "path": "Main.java" },
                "breakpoints": [ { "line": 3, "hitCondition": "== 2" } ],
            }),
        )
        .await;
    assert!(
        bp_resp
            .get("success")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        "setBreakpoints was not successful: {bp_resp}"
    );

    let thread_id = client.first_thread_id().await;
    let (_pause_resp, _pause_stopped) = client.pause(Some(thread_id)).await;
    let frame_id = client.first_frame_id(thread_id).await;

    // While stream debug is running, the mock will emit a breakpoint event on the evaluation
    // thread. The adapter should resume the thread and suppress any DAP stop/output events.
    let mut events = client.subscribe_events();
    let stream_req = client
        .send_request(
            "nova/streamDebug",
            json!({
                "expression": "list.stream().map(x -> x).count()",
                "frameId": frame_id,
            }),
        )
        .await;

    let mut resp_fut =
        Box::pin(client.wait_for_response_with_timeout(stream_req, Duration::from_secs(2)));

    let stream_resp = loop {
        tokio::select! {
            resp = &mut resp_fut => break resp,
            evt = events.recv() => {
                let evt = evt.expect("event channel closed");
                if evt.get("event").and_then(|v| v.as_str()) == Some("stopped") {
                    panic!("streamDebug should not emit stopped events during internal eval: {evt}");
                }
                if evt.get("event").and_then(|v| v.as_str()) == Some("output") {
                    panic!("streamDebug should not emit output events during internal eval: {evt}");
                }
            }
        }
    };
    assert!(
        stream_resp
            .get("success")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        "streamDebug response was not successful: {stream_resp}"
    );

    // Ensure the breakpoint's hit count was not affected by the internal breakpoint event:
    // resuming the program should hit the breakpoint once (hit count = 1) and auto-continue,
    // without producing a stopped event.
    let (_cont_resp, _continued) = client.continue_with_thread_id(Some(thread_id)).await;
    assert_eq!(jdwp.breakpoint_suspend_policy().await, Some(1));

    let stopped =
        tokio::time::timeout(Duration::from_millis(200), client.wait_for_event("stopped")).await;
    assert!(
        stopped.is_err(),
        "hit-count breakpoint should not stop after streamDebug internal eval"
    );

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}
