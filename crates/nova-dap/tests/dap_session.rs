use serde_json::{json, Value};
use std::time::Duration;

use base64::{engine::general_purpose, Engine as _};
use nova_dap::dap_tokio::{DapReader, DapWriter};
use nova_dap::object_registry::{OBJECT_HANDLE_BASE, PINNED_SCOPE_REF};
use nova_dap::wire_server;
use nova_jdwp::wire::mock::MockJdwpServer;

async fn send_request(
    writer: &mut DapWriter<tokio::io::WriteHalf<tokio::io::DuplexStream>>,
    seq: i64,
    command: &str,
    arguments: Value,
) {
    let msg = json!({
        "seq": seq,
        "type": "request",
        "command": command,
        "arguments": arguments,
    });
    writer.write_value(&msg).await.unwrap();
}

async fn read_next(reader: &mut DapReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>) -> Value {
    reader.read_value().await.unwrap().unwrap()
}

async fn read_response(
    reader: &mut DapReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    request_seq: i64,
) -> Value {
    for _ in 0..50 {
        let msg = read_next(reader).await;
        if msg.get("type").and_then(|v| v.as_str()) == Some("response")
            && msg.get("request_seq").and_then(|v| v.as_i64()) == Some(request_seq)
        {
            return msg;
        }
    }
    panic!("did not receive response for seq {request_seq}");
}

async fn read_response_and_event(
    reader: &mut DapReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    request_seq: i64,
    event: &str,
) -> (Value, Value) {
    let mut response = None;
    let mut event_msg = None;

    for _ in 0..200 {
        let msg = read_next(reader).await;

        match msg.get("type").and_then(|v| v.as_str()) {
            Some("response")
                if msg.get("request_seq").and_then(|v| v.as_i64()) == Some(request_seq) =>
            {
                response = Some(msg);
            }
            Some("event") if msg.get("event").and_then(|v| v.as_str()) == Some(event) => {
                event_msg = Some(msg);
            }
            _ => {}
        }

        if let (Some(resp), Some(evt)) = (response.clone(), event_msg.clone()) {
            return (resp, evt);
        }
    }

    panic!("did not receive response+event for seq {request_seq} ({event})");
}

#[tokio::test]
async fn dap_can_attach_set_breakpoints_and_stop() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();

    let (client, server_stream) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_stream);
    let server_task =
        tokio::spawn(async move { wire_server::run(server_read, server_write).await });

    let (client_read, client_write) = tokio::io::split(client);
    let mut reader = DapReader::new(client_read);
    let mut writer = DapWriter::new(client_write);

    send_request(&mut writer, 1, "initialize", json!({})).await;
    let init_resp = read_response(&mut reader, 1).await;
    assert!(init_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));
    // Initialized event.
    let initialized = read_next(&mut reader).await;
    assert_eq!(
        initialized.get("event").and_then(|v| v.as_str()),
        Some("initialized")
    );

    send_request(
        &mut writer,
        2,
        "attach",
        json!({
            "host": "127.0.0.1",
            "port": jdwp.addr().port()
        }),
    )
    .await;
    let attach_resp = read_response(&mut reader, 2).await;
    assert!(attach_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

    send_request(
        &mut writer,
        3,
        "setBreakpoints",
        json!({
            "source": { "path": "Main.java" },
            "breakpoints": [ { "line": 3 } ]
        }),
    )
    .await;
    let bp_resp = read_response(&mut reader, 3).await;
    let verified = bp_resp
        .pointer("/body/breakpoints/0/verified")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    assert!(verified);
    assert_eq!(jdwp.breakpoint_suspend_policy().await, Some(1));

    send_request(&mut writer, 4, "threads", json!({})).await;
    let threads_resp = read_response(&mut reader, 4).await;
    let thread_id = threads_resp
        .pointer("/body/threads/0/id")
        .and_then(|v| v.as_i64())
        .unwrap();

    send_request(
        &mut writer,
        5,
        "stackTrace",
        json!({ "threadId": thread_id }),
    )
    .await;
    let stack_resp = read_response(&mut reader, 5).await;
    let frame_id = stack_resp
        .pointer("/body/stackFrames/0/id")
        .and_then(|v| v.as_i64())
        .unwrap();

    send_request(&mut writer, 6, "scopes", json!({ "frameId": frame_id })).await;
    let scopes_resp = read_response(&mut reader, 6).await;
    let locals_ref = scopes_resp
        .pointer("/body/scopes/0/variablesReference")
        .and_then(|v| v.as_i64())
        .unwrap();

    send_request(
        &mut writer,
        7,
        "variables",
        json!({ "variablesReference": locals_ref }),
    )
    .await;
    let vars_resp = read_response(&mut reader, 7).await;
    let locals = vars_resp
        .pointer("/body/variables")
        .and_then(|v| v.as_array())
        .unwrap();
    assert!(locals
        .iter()
        .any(|v| v.get("name").and_then(|n| n.as_str()) == Some("x")));

    // Pause should suspend the VM and emit a stopped event.
    send_request(&mut writer, 8, "pause", json!({ "threadId": thread_id })).await;
    let mut pause_resp = None;
    let mut pause_stopped = None;
    for _ in 0..50 {
        let msg = read_next(&mut reader).await;
        if msg.get("type").and_then(|v| v.as_str()) == Some("response")
            && msg.get("request_seq").and_then(|v| v.as_i64()) == Some(8)
        {
            pause_resp = Some(msg);
        } else if msg.get("type").and_then(|v| v.as_str()) == Some("event")
            && msg.get("event").and_then(|v| v.as_str()) == Some("stopped")
            && msg.pointer("/body/reason").and_then(|v| v.as_str()) == Some("pause")
        {
            pause_stopped = Some(msg);
        }

        if pause_resp.is_some() && pause_stopped.is_some() {
            break;
        }
    }
    let pause_resp = pause_resp.expect("expected pause response");
    assert!(pause_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));
    let pause_stopped = pause_stopped.expect("expected stopped event for pause");
    assert_eq!(
        pause_stopped
            .pointer("/body/allThreadsStopped")
            .and_then(|v| v.as_bool()),
        Some(false)
    );
    assert_eq!(jdwp.thread_suspend_calls(), 1);
    assert_eq!(jdwp.vm_suspend_calls(), 0);

    // Unknown/unhandled requests should be reported as errors (success: false).
    send_request(&mut writer, 9, "nope", json!({})).await;
    let bad_resp = read_response(&mut reader, 9).await;
    assert!(!bad_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(true));

    // Continue should emit a continued event and then a stopped event from the mock JDWP VM.
    send_request(
        &mut writer,
        10,
        "continue",
        json!({ "threadId": thread_id }),
    )
    .await;

    let mut cont_resp = None;
    let mut continued = None;
    let mut stopped = None;

    for _ in 0..100 {
        let msg = read_next(&mut reader).await;

        if msg.get("type").and_then(|v| v.as_str()) == Some("response")
            && msg.get("request_seq").and_then(|v| v.as_i64()) == Some(10)
        {
            cont_resp = Some(msg);
        } else if msg.get("type").and_then(|v| v.as_str()) == Some("event")
            && msg.get("event").and_then(|v| v.as_str()) == Some("continued")
        {
            continued = Some(msg);
        } else if msg.get("type").and_then(|v| v.as_str()) == Some("event")
            && msg.get("event").and_then(|v| v.as_str()) == Some("stopped")
            && msg.pointer("/body/reason").and_then(|v| v.as_str()) == Some("breakpoint")
        {
            stopped = Some(msg);
        }

        if cont_resp.is_some() && continued.is_some() && stopped.is_some() {
            break;
        }
    }

    let cont_resp = cont_resp.expect("expected continue response");
    assert!(cont_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));
    assert_eq!(jdwp.thread_resume_calls(), 1);
    assert_eq!(jdwp.vm_resume_calls(), 0);
    assert_eq!(
        cont_resp
            .pointer("/body/allThreadsContinued")
            .and_then(|v| v.as_bool()),
        Some(false)
    );

    let continued = continued.expect("expected continued event");
    assert_eq!(
        continued
            .pointer("/body/allThreadsContinued")
            .and_then(|v| v.as_bool()),
        Some(false)
    );

    let stopped = stopped.expect("expected stopped event");
    assert_eq!(
        stopped.pointer("/body/reason").and_then(|v| v.as_str()),
        Some("breakpoint")
    );
    // The mock JDWP VM uses SuspendPolicy.EVENT_THREAD (only the event thread is suspended).
    assert_eq!(
        stopped
            .pointer("/body/allThreadsStopped")
            .and_then(|v| v.as_bool()),
        Some(false)
    );

    send_request(&mut writer, 11, "disconnect", json!({})).await;
    let _disc_resp = read_response(&mut reader, 11).await;

    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_can_hot_swap_a_class() {
    let mut caps = vec![false; 32];
    caps[7] = true; // canRedefineClasses
    let jdwp = MockJdwpServer::spawn_with_capabilities(caps).await.unwrap();

    let (client, server_stream) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_stream);
    let server_task =
        tokio::spawn(async move { wire_server::run(server_read, server_write).await });

    let (client_read, client_write) = tokio::io::split(client);
    let mut reader = DapReader::new(client_read);
    let mut writer = DapWriter::new(client_write);

    send_request(&mut writer, 1, "initialize", json!({})).await;
    let init_resp = read_response(&mut reader, 1).await;
    assert!(init_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));
    let _initialized = read_next(&mut reader).await;

    send_request(
        &mut writer,
        2,
        "attach",
        json!({
            "host": "127.0.0.1",
            "port": jdwp.addr().port()
        }),
    )
    .await;
    let attach_resp = read_response(&mut reader, 2).await;
    assert!(attach_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

    let bytecode = vec![0xCA, 0xFE];
    let bytecode_base64 = general_purpose::STANDARD.encode(&bytecode);
    send_request(
        &mut writer,
        3,
        "nova/hotSwap",
        json!({
            "classes": [{
                "className": "Main",
                "bytecodeBase64": bytecode_base64
            }]
        }),
    )
    .await;
    let hot_swap_resp = read_response(&mut reader, 3).await;
    assert!(hot_swap_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));
    assert_eq!(
        hot_swap_resp
            .pointer("/body/results/0/status")
            .and_then(|v| v.as_str()),
        Some("success")
    );
    assert_eq!(
        hot_swap_resp
            .pointer("/body/results/0/file")
            .and_then(|v| v.as_str()),
        Some("Main.java")
    );

    let calls = jdwp.redefine_classes_calls().await;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].class_count, 1);
    assert_eq!(calls[0].classes.len(), 1);
    assert_eq!(calls[0].classes[0].1, bytecode);

    send_request(&mut writer, 4, "disconnect", json!({})).await;
    let _disc_resp = read_response(&mut reader, 4).await;

    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_hot_swap_reports_schema_change() {
    let mut caps = vec![false; 32];
    caps[7] = true; // canRedefineClasses
    let jdwp = MockJdwpServer::spawn_with_capabilities(caps).await.unwrap();
    jdwp.set_redefine_classes_error_code(62); // SCHEMA_CHANGE_NOT_IMPLEMENTED

    let (client, server_stream) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_stream);
    let server_task =
        tokio::spawn(async move { wire_server::run(server_read, server_write).await });

    let (client_read, client_write) = tokio::io::split(client);
    let mut reader = DapReader::new(client_read);
    let mut writer = DapWriter::new(client_write);

    send_request(&mut writer, 1, "initialize", json!({})).await;
    let _init_resp = read_response(&mut reader, 1).await;
    let _initialized = read_next(&mut reader).await;

    send_request(
        &mut writer,
        2,
        "attach",
        json!({
            "host": "127.0.0.1",
            "port": jdwp.addr().port()
        }),
    )
    .await;
    let attach_resp = read_response(&mut reader, 2).await;
    assert!(attach_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

    let bytecode_base64 = general_purpose::STANDARD.encode([0u8; 4]);
    send_request(
        &mut writer,
        3,
        "nova/hotSwap",
        json!({
            "classes": [{
                "className": "Main",
                "bytecodeBase64": bytecode_base64
            }]
        }),
    )
    .await;
    let hot_swap_resp = read_response(&mut reader, 3).await;
    assert!(hot_swap_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));
    assert_eq!(
        hot_swap_resp
            .pointer("/body/results/0/status")
            .and_then(|v| v.as_str()),
        Some("schema_change")
    );
    let msg = hot_swap_resp
        .pointer("/body/results/0/message")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(msg.contains("JDWP error 62"), "unexpected message: {msg:?}");

    send_request(&mut writer, 4, "disconnect", json!({})).await;
    let _disc_resp = read_response(&mut reader, 4).await;

    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_wire_handle_tables_are_stable_within_stop_and_invalidated_on_resume() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();

    let (client, server_stream) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_stream);
    let server_task =
        tokio::spawn(async move { wire_server::run(server_read, server_write).await });

    let (client_read, client_write) = tokio::io::split(client);
    let mut reader = DapReader::new(client_read);
    let mut writer = DapWriter::new(client_write);

    send_request(&mut writer, 1, "initialize", json!({})).await;
    let init_resp = read_response(&mut reader, 1).await;
    assert!(init_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));
    let initialized = read_next(&mut reader).await;
    assert_eq!(
        initialized.get("event").and_then(|v| v.as_str()),
        Some("initialized")
    );

    send_request(
        &mut writer,
        2,
        "attach",
        json!({
            "host": "127.0.0.1",
            "port": jdwp.addr().port()
        }),
    )
    .await;
    let attach_resp = read_response(&mut reader, 2).await;
    assert!(attach_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

    send_request(
        &mut writer,
        3,
        "setBreakpoints",
        json!({
            "source": { "path": "Main.java" },
            "breakpoints": [ { "line": 3 } ]
        }),
    )
    .await;
    let bp_resp = read_response(&mut reader, 3).await;
    assert!(bp_resp
        .pointer("/body/breakpoints/0/verified")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));
    assert_eq!(jdwp.breakpoint_suspend_policy().await, Some(1));

    send_request(&mut writer, 4, "threads", json!({})).await;
    let threads_resp = read_response(&mut reader, 4).await;
    let thread_id = threads_resp
        .pointer("/body/threads/0/id")
        .and_then(|v| v.as_i64())
        .unwrap();

    // Continue to generate an initial stop.
    send_request(&mut writer, 5, "continue", json!({ "threadId": thread_id })).await;
    let mut cont_resp = None;
    let mut stopped = None;
    for _ in 0..100 {
        let msg = read_next(&mut reader).await;
        if msg.get("type").and_then(|v| v.as_str()) == Some("response")
            && msg.get("request_seq").and_then(|v| v.as_i64()) == Some(5)
        {
            cont_resp = Some(msg);
        } else if msg.get("type").and_then(|v| v.as_str()) == Some("event")
            && msg.get("event").and_then(|v| v.as_str()) == Some("stopped")
            && msg.pointer("/body/reason").and_then(|v| v.as_str()) == Some("breakpoint")
        {
            stopped = Some(msg);
        }

        if cont_resp.is_some() && stopped.is_some() {
            break;
        }
    }
    assert!(cont_resp
        .expect("expected continue response")
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));
    let stopped = stopped.expect("expected stopped event");
    assert_eq!(
        stopped
            .pointer("/body/allThreadsStopped")
            .and_then(|v| v.as_bool()),
        Some(false)
    );

    // Repeated stackTrace calls should return stable frame ids.
    send_request(
        &mut writer,
        6,
        "stackTrace",
        json!({ "threadId": thread_id }),
    )
    .await;
    let stack_a = read_response(&mut reader, 6).await;
    let frame_id_a = stack_a
        .pointer("/body/stackFrames/0/id")
        .and_then(|v| v.as_i64())
        .unwrap();

    send_request(
        &mut writer,
        7,
        "stackTrace",
        json!({ "threadId": thread_id }),
    )
    .await;
    let stack_b = read_response(&mut reader, 7).await;
    let frame_id_b = stack_b
        .pointer("/body/stackFrames/0/id")
        .and_then(|v| v.as_i64())
        .unwrap();
    assert_eq!(frame_id_a, frame_id_b);

    // And repeated scopes calls should return stable locals handles.
    send_request(&mut writer, 8, "scopes", json!({ "frameId": frame_id_a })).await;
    let scopes_a = read_response(&mut reader, 8).await;
    let locals_ref_a = scopes_a
        .pointer("/body/scopes/0/variablesReference")
        .and_then(|v| v.as_i64())
        .unwrap();

    send_request(&mut writer, 9, "scopes", json!({ "frameId": frame_id_a })).await;
    let scopes_b = read_response(&mut reader, 9).await;
    let locals_ref_b = scopes_b
        .pointer("/body/scopes/0/variablesReference")
        .and_then(|v| v.as_i64())
        .unwrap();
    assert_eq!(locals_ref_a, locals_ref_b);

    // Resume; the next stop should allocate fresh handles (stale ids must not alias).
    send_request(
        &mut writer,
        10,
        "continue",
        json!({ "threadId": thread_id }),
    )
    .await;
    let mut cont_resp = None;
    let mut stopped = None;
    for _ in 0..100 {
        let msg = read_next(&mut reader).await;
        if msg.get("type").and_then(|v| v.as_str()) == Some("response")
            && msg.get("request_seq").and_then(|v| v.as_i64()) == Some(10)
        {
            cont_resp = Some(msg);
        } else if msg.get("type").and_then(|v| v.as_str()) == Some("event")
            && msg.get("event").and_then(|v| v.as_str()) == Some("stopped")
            && msg.pointer("/body/reason").and_then(|v| v.as_str()) == Some("breakpoint")
        {
            stopped = Some(msg);
        }

        if cont_resp.is_some() && stopped.is_some() {
            break;
        }
    }
    assert!(cont_resp
        .expect("expected continue response")
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));
    assert!(stopped.is_some());

    send_request(
        &mut writer,
        11,
        "stackTrace",
        json!({ "threadId": thread_id }),
    )
    .await;
    let stack_after = read_response(&mut reader, 11).await;
    let frame_id_after = stack_after
        .pointer("/body/stackFrames/0/id")
        .and_then(|v| v.as_i64())
        .unwrap();
    assert_ne!(frame_id_a, frame_id_after);

    send_request(
        &mut writer,
        12,
        "scopes",
        json!({ "frameId": frame_id_after }),
    )
    .await;
    let scopes_after = read_response(&mut reader, 12).await;
    let locals_ref_after = scopes_after
        .pointer("/body/scopes/0/variablesReference")
        .and_then(|v| v.as_i64())
        .unwrap();
    assert_ne!(locals_ref_a, locals_ref_after);

    // Old frame ids should be rejected rather than resolving to a different frame.
    send_request(&mut writer, 13, "scopes", json!({ "frameId": frame_id_a })).await;
    let stale_scopes = read_response(&mut reader, 13).await;
    assert_eq!(
        stale_scopes.get("success").and_then(|v| v.as_bool()),
        Some(false)
    );

    // Old variables references should return empty results.
    send_request(
        &mut writer,
        14,
        "variables",
        json!({ "variablesReference": locals_ref_a }),
    )
    .await;
    let stale_vars = read_response(&mut reader, 14).await;
    let vars = stale_vars
        .pointer("/body/variables")
        .and_then(|v| v.as_array())
        .unwrap();
    assert!(vars.is_empty());

    send_request(&mut writer, 15, "disconnect", json!({})).await;
    let _disc_resp = read_response(&mut reader, 15).await;

    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_object_handles_are_stable_across_stops_and_pinning_exposes_them_in_a_scope() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();

    let (client, server_stream) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_stream);
    let server_task =
        tokio::spawn(async move { wire_server::run(server_read, server_write).await });

    let (client_read, client_write) = tokio::io::split(client);
    let mut reader = DapReader::new(client_read);
    let mut writer = DapWriter::new(client_write);

    send_request(&mut writer, 1, "initialize", json!({})).await;
    let init_resp = read_response(&mut reader, 1).await;
    assert!(init_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));
    let initialized = read_next(&mut reader).await;
    assert_eq!(
        initialized.get("event").and_then(|v| v.as_str()),
        Some("initialized")
    );

    send_request(
        &mut writer,
        2,
        "attach",
        json!({
            "host": "127.0.0.1",
            "port": jdwp.addr().port()
        }),
    )
    .await;
    let attach_resp = read_response(&mut reader, 2).await;
    assert!(attach_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

    send_request(
        &mut writer,
        3,
        "setBreakpoints",
        json!({
            "source": { "path": "Main.java" },
            "breakpoints": [ { "line": 3 } ]
        }),
    )
    .await;
    let bp_resp = read_response(&mut reader, 3).await;
    assert!(bp_resp
        .pointer("/body/breakpoints/0/verified")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

    send_request(&mut writer, 4, "threads", json!({})).await;
    let threads_resp = read_response(&mut reader, 4).await;
    let thread_id = threads_resp
        .pointer("/body/threads/0/id")
        .and_then(|v| v.as_i64())
        .unwrap();

    // First stop.
    send_request(&mut writer, 5, "continue", json!({ "threadId": thread_id })).await;
    let (_continue_resp, _stopped) = read_response_and_event(&mut reader, 5, "stopped").await;

    send_request(
        &mut writer,
        6,
        "stackTrace",
        json!({ "threadId": thread_id }),
    )
    .await;
    let stack_resp = read_response(&mut reader, 6).await;
    let frame_id = stack_resp
        .pointer("/body/stackFrames/0/id")
        .and_then(|v| v.as_i64())
        .unwrap();

    send_request(&mut writer, 7, "scopes", json!({ "frameId": frame_id })).await;
    let scopes_resp = read_response(&mut reader, 7).await;
    let locals_ref = scopes_resp
        .pointer("/body/scopes/0/variablesReference")
        .and_then(|v| v.as_i64())
        .unwrap();

    send_request(
        &mut writer,
        8,
        "variables",
        json!({ "variablesReference": locals_ref }),
    )
    .await;
    let vars_resp = read_response(&mut reader, 8).await;
    let locals = vars_resp
        .pointer("/body/variables")
        .and_then(|v| v.as_array())
        .unwrap();
    let obj_ref = locals
        .iter()
        .find(|v| v.get("name").and_then(|n| n.as_str()) == Some("obj"))
        .and_then(|v| v.get("variablesReference"))
        .and_then(|v| v.as_i64())
        .unwrap();
    assert!(obj_ref > OBJECT_HANDLE_BASE);

    // Not pinned: object handles should remain stable across resumes as long as the
    // underlying object is still alive.
    send_request(&mut writer, 9, "continue", json!({ "threadId": thread_id })).await;
    let (_continue_resp, _stopped) = read_response_and_event(&mut reader, 9, "stopped").await;

    send_request(
        &mut writer,
        10,
        "variables",
        json!({ "variablesReference": obj_ref }),
    )
    .await;
    let stale_obj_vars = read_response(&mut reader, 10).await;
    let stale = stale_obj_vars
        .pointer("/body/variables")
        .and_then(|v| v.as_array())
        .unwrap();
    assert!(!stale.is_empty());

    // Pin a fresh object handle.
    send_request(
        &mut writer,
        11,
        "stackTrace",
        json!({ "threadId": thread_id }),
    )
    .await;
    let stack_resp = read_response(&mut reader, 11).await;
    let frame_id = stack_resp
        .pointer("/body/stackFrames/0/id")
        .and_then(|v| v.as_i64())
        .unwrap();

    send_request(&mut writer, 12, "scopes", json!({ "frameId": frame_id })).await;
    let scopes_resp = read_response(&mut reader, 12).await;
    let locals_ref = scopes_resp
        .pointer("/body/scopes/0/variablesReference")
        .and_then(|v| v.as_i64())
        .unwrap();
    let pinned_ref = scopes_resp
        .pointer("/body/scopes/1/variablesReference")
        .and_then(|v| v.as_i64())
        .unwrap();
    assert_eq!(pinned_ref, PINNED_SCOPE_REF);

    send_request(
        &mut writer,
        13,
        "variables",
        json!({ "variablesReference": locals_ref }),
    )
    .await;
    let vars_resp = read_response(&mut reader, 13).await;
    let locals = vars_resp
        .pointer("/body/variables")
        .and_then(|v| v.as_array())
        .unwrap();
    let obj_ref = locals
        .iter()
        .find(|v| v.get("name").and_then(|n| n.as_str()) == Some("obj"))
        .and_then(|v| v.get("variablesReference"))
        .and_then(|v| v.as_i64())
        .unwrap();

    send_request(
        &mut writer,
        14,
        "nova/pinObject",
        json!({ "variablesReference": obj_ref, "pinned": true }),
    )
    .await;
    let pin_resp = read_response(&mut reader, 14).await;
    assert_eq!(
        pin_resp.pointer("/body/pinned").and_then(|v| v.as_bool()),
        Some(true)
    );

    // Resume again; pinned handle must survive.
    send_request(
        &mut writer,
        15,
        "continue",
        json!({ "threadId": thread_id }),
    )
    .await;
    let (_continue_resp, _stopped) = read_response_and_event(&mut reader, 15, "stopped").await;

    send_request(
        &mut writer,
        16,
        "variables",
        json!({ "variablesReference": PINNED_SCOPE_REF }),
    )
    .await;
    let pinned_vars_resp = read_response(&mut reader, 16).await;
    let pinned_vars = pinned_vars_resp
        .pointer("/body/variables")
        .and_then(|v| v.as_array())
        .unwrap();
    assert!(pinned_vars
        .iter()
        .any(|v| v.get("variablesReference").and_then(|v| v.as_i64()) == Some(obj_ref)));

    send_request(
        &mut writer,
        17,
        "variables",
        json!({ "variablesReference": obj_ref }),
    )
    .await;
    let obj_vars = read_response(&mut reader, 17).await;
    let fields = obj_vars
        .pointer("/body/variables")
        .and_then(|v| v.as_array())
        .unwrap();
    assert!(!fields.is_empty());

    send_request(&mut writer, 18, "disconnect", json!({})).await;
    let _disc_resp = read_response(&mut reader, 18).await;

    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_step_stop_uses_event_thread_suspend_policy() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();

    let (client, server_stream) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_stream);
    let server_task =
        tokio::spawn(async move { wire_server::run(server_read, server_write).await });

    let (client_read, client_write) = tokio::io::split(client);
    let mut reader = DapReader::new(client_read);
    let mut writer = DapWriter::new(client_write);

    send_request(&mut writer, 1, "initialize", json!({})).await;
    let init_resp = read_response(&mut reader, 1).await;
    assert!(init_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));
    let initialized = read_next(&mut reader).await;
    assert_eq!(
        initialized.get("event").and_then(|v| v.as_str()),
        Some("initialized")
    );

    send_request(
        &mut writer,
        2,
        "attach",
        json!({
            "host": "127.0.0.1",
            "port": jdwp.addr().port()
        }),
    )
    .await;
    let attach_resp = read_response(&mut reader, 2).await;
    assert!(attach_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

    send_request(&mut writer, 3, "threads", json!({})).await;
    let threads_resp = read_response(&mut reader, 3).await;
    let thread_id = threads_resp
        .pointer("/body/threads/0/id")
        .and_then(|v| v.as_i64())
        .unwrap();

    send_request(&mut writer, 4, "next", json!({ "threadId": thread_id })).await;
    let mut next_resp = None;
    let mut stopped = None;
    for _ in 0..100 {
        let msg = read_next(&mut reader).await;
        if msg.get("type").and_then(|v| v.as_str()) == Some("response")
            && msg.get("request_seq").and_then(|v| v.as_i64()) == Some(4)
        {
            next_resp = Some(msg);
        } else if msg.get("type").and_then(|v| v.as_str()) == Some("event")
            && msg.get("event").and_then(|v| v.as_str()) == Some("stopped")
            && msg.pointer("/body/reason").and_then(|v| v.as_str()) == Some("step")
        {
            stopped = Some(msg);
        }

        if next_resp.is_some() && stopped.is_some() {
            break;
        }
    }
    assert!(next_resp
        .expect("expected next response")
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));
    assert_eq!(jdwp.thread_resume_calls(), 1);
    assert_eq!(jdwp.vm_resume_calls(), 0);
    assert_eq!(jdwp.step_suspend_policy().await, Some(1));
    let stopped = stopped.expect("expected stopped event");
    assert_eq!(
        stopped
            .pointer("/body/allThreadsStopped")
            .and_then(|v| v.as_bool()),
        Some(false)
    );

    send_request(&mut writer, 5, "disconnect", json!({})).await;
    let _disc_resp = read_response(&mut reader, 5).await;

    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_can_expand_object_fields_and_pin_objects() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();

    let (client, server_stream) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_stream);
    let server_task =
        tokio::spawn(async move { wire_server::run(server_read, server_write).await });

    let (client_read, client_write) = tokio::io::split(client);
    let mut reader = DapReader::new(client_read);
    let mut writer = DapWriter::new(client_write);

    send_request(&mut writer, 1, "initialize", json!({})).await;
    let init_resp = read_response(&mut reader, 1).await;
    assert!(init_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));
    let initialized = read_next(&mut reader).await;
    assert_eq!(
        initialized.get("event").and_then(|v| v.as_str()),
        Some("initialized")
    );

    send_request(
        &mut writer,
        2,
        "attach",
        json!({
            "host": "127.0.0.1",
            "port": jdwp.addr().port()
        }),
    )
    .await;
    let attach_resp = read_response(&mut reader, 2).await;
    assert!(attach_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

    send_request(&mut writer, 3, "threads", json!({})).await;
    let threads_resp = read_response(&mut reader, 3).await;
    let thread_id = threads_resp
        .pointer("/body/threads/0/id")
        .and_then(|v| v.as_i64())
        .unwrap();

    send_request(
        &mut writer,
        4,
        "stackTrace",
        json!({ "threadId": thread_id }),
    )
    .await;
    let stack_resp = read_response(&mut reader, 4).await;
    let frame_id = stack_resp
        .pointer("/body/stackFrames/0/id")
        .and_then(|v| v.as_i64())
        .unwrap();

    send_request(&mut writer, 5, "scopes", json!({ "frameId": frame_id })).await;
    let scopes_resp = read_response(&mut reader, 5).await;
    let locals_ref = scopes_resp
        .pointer("/body/scopes/0/variablesReference")
        .and_then(|v| v.as_i64())
        .unwrap();
    let pinned_ref = scopes_resp
        .pointer("/body/scopes/1/variablesReference")
        .and_then(|v| v.as_i64())
        .unwrap();
    assert_eq!(pinned_ref, PINNED_SCOPE_REF);

    send_request(
        &mut writer,
        6,
        "variables",
        json!({ "variablesReference": locals_ref }),
    )
    .await;
    let vars_resp = read_response(&mut reader, 6).await;
    let locals = vars_resp
        .pointer("/body/variables")
        .and_then(|v| v.as_array())
        .unwrap();

    let obj = locals
        .iter()
        .find(|v| v.get("name").and_then(|n| n.as_str()) == Some("obj"))
        .expect("expected locals to contain obj");
    let obj_ref = obj
        .get("variablesReference")
        .and_then(|v| v.as_i64())
        .unwrap();
    assert!(
        obj_ref > OBJECT_HANDLE_BASE,
        "expected stable object handle variablesReference"
    );
    assert!(obj
        .get("value")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .contains('@'));

    send_request(
        &mut writer,
        7,
        "variables",
        json!({ "variablesReference": obj_ref }),
    )
    .await;
    let fields_resp = read_response(&mut reader, 7).await;
    let fields = fields_resp
        .pointer("/body/variables")
        .and_then(|v| v.as_array())
        .unwrap();
    let field = fields
        .iter()
        .find(|v| v.get("name").and_then(|n| n.as_str()) == Some("field"))
        .expect("expected object to contain field");
    assert_eq!(field.get("value").and_then(|v| v.as_str()), Some("7"));
    assert_eq!(field.get("type").and_then(|v| v.as_str()), Some("int"));

    // Pin the object.
    send_request(
        &mut writer,
        8,
        "nova/pinObject",
        json!({ "variablesReference": obj_ref, "pinned": true }),
    )
    .await;
    let pin_resp = read_response(&mut reader, 8).await;
    assert_eq!(
        pin_resp.pointer("/body/pinned").and_then(|v| v.as_bool()),
        Some(true)
    );

    // Pinned objects are visible under the synthetic scope.
    send_request(
        &mut writer,
        9,
        "variables",
        json!({ "variablesReference": PINNED_SCOPE_REF }),
    )
    .await;
    let pinned_vars_resp = read_response(&mut reader, 9).await;
    let pinned_vars = pinned_vars_resp
        .pointer("/body/variables")
        .and_then(|v| v.as_array())
        .unwrap();
    assert_eq!(pinned_vars.len(), 1);
    assert_eq!(
        pinned_vars[0]
            .get("variablesReference")
            .and_then(|v| v.as_i64()),
        Some(obj_ref)
    );
    assert_eq!(
        pinned_vars[0]
            .get("presentationHint")
            .and_then(|v| v.get("attributes"))
            .and_then(|v| v.get(0))
            .and_then(|v| v.as_str()),
        Some("pinned")
    );

    // Unpin the object.
    send_request(
        &mut writer,
        10,
        "nova/pinObject",
        json!({ "variablesReference": obj_ref, "pinned": false }),
    )
    .await;
    let unpin_resp = read_response(&mut reader, 10).await;
    assert_eq!(
        unpin_resp.pointer("/body/pinned").and_then(|v| v.as_bool()),
        Some(false)
    );

    send_request(
        &mut writer,
        11,
        "variables",
        json!({ "variablesReference": PINNED_SCOPE_REF }),
    )
    .await;
    let pinned_empty_resp = read_response(&mut reader, 11).await;
    let pinned_empty = pinned_empty_resp
        .pointer("/body/variables")
        .and_then(|v| v.as_array())
        .unwrap();
    assert!(pinned_empty.is_empty());

    send_request(&mut writer, 12, "disconnect", json!({})).await;
    let _disc_resp = read_response(&mut reader, 12).await;

    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_exception_info_includes_type_name() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();

    let (client, server_stream) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_stream);
    let server_task =
        tokio::spawn(async move { wire_server::run(server_read, server_write).await });

    let (client_read, client_write) = tokio::io::split(client);
    let mut reader = DapReader::new(client_read);
    let mut writer = DapWriter::new(client_write);

    send_request(&mut writer, 1, "initialize", json!({})).await;
    let init_resp = read_response(&mut reader, 1).await;
    assert!(init_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));
    assert_eq!(
        init_resp
            .pointer("/body/supportsExceptionInfoRequest")
            .and_then(|v| v.as_bool()),
        Some(true)
    );
    let initialized = read_next(&mut reader).await;
    assert_eq!(
        initialized.get("event").and_then(|v| v.as_str()),
        Some("initialized")
    );

    send_request(
        &mut writer,
        2,
        "attach",
        json!({
            "host": "127.0.0.1",
            "port": jdwp.addr().port()
        }),
    )
    .await;
    let attach_resp = read_response(&mut reader, 2).await;
    assert!(attach_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

    send_request(
        &mut writer,
        3,
        "setExceptionBreakpoints",
        json!({
            "filters": ["all"]
        }),
    )
    .await;
    let exc_bp_resp = read_response(&mut reader, 3).await;
    assert!(exc_bp_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

    send_request(&mut writer, 4, "threads", json!({})).await;
    let threads_resp = read_response(&mut reader, 4).await;
    let thread_id = threads_resp
        .pointer("/body/threads/0/id")
        .and_then(|v| v.as_i64())
        .unwrap();

    send_request(&mut writer, 5, "continue", json!({ "threadId": thread_id })).await;

    let mut cont_resp = None;
    let mut continued = None;
    let mut stopped = None;

    for _ in 0..100 {
        let msg = read_next(&mut reader).await;

        if msg.get("type").and_then(|v| v.as_str()) == Some("response")
            && msg.get("request_seq").and_then(|v| v.as_i64()) == Some(5)
        {
            cont_resp = Some(msg);
        } else if msg.get("type").and_then(|v| v.as_str()) == Some("event")
            && msg.get("event").and_then(|v| v.as_str()) == Some("continued")
        {
            continued = Some(msg);
        } else if msg.get("type").and_then(|v| v.as_str()) == Some("event")
            && msg.get("event").and_then(|v| v.as_str()) == Some("stopped")
            && msg.pointer("/body/reason").and_then(|v| v.as_str()) == Some("exception")
        {
            stopped = Some(msg);
        }

        if cont_resp.is_some() && continued.is_some() && stopped.is_some() {
            break;
        }
    }

    let cont_resp = cont_resp.expect("expected continue response");
    assert!(cont_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

    let stopped = stopped.expect("expected stopped event");
    assert_eq!(
        stopped.pointer("/body/reason").and_then(|v| v.as_str()),
        Some("exception")
    );

    send_request(
        &mut writer,
        6,
        "exceptionInfo",
        json!({ "threadId": thread_id }),
    )
    .await;
    let exc_info = read_response(&mut reader, 6).await;
    assert!(exc_info
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));
    assert_eq!(
        exc_info
            .pointer("/body/exceptionId")
            .and_then(|v| v.as_str()),
        Some("java.lang.RuntimeException")
    );
    assert_eq!(
        exc_info
            .pointer("/body/details/fullTypeName")
            .and_then(|v| v.as_str()),
        Some("java.lang.RuntimeException")
    );
    assert_eq!(
        exc_info
            .pointer("/body/details/typeName")
            .and_then(|v| v.as_str()),
        Some("RuntimeException")
    );
    assert_eq!(
        exc_info.pointer("/body/breakMode").and_then(|v| v.as_str()),
        Some("always")
    );
    assert_eq!(
        exc_info
            .pointer("/body/description")
            .and_then(|v| v.as_str()),
        Some("mock string")
    );
    assert_eq!(
        exc_info
            .pointer("/body/details/message")
            .and_then(|v| v.as_str()),
        Some("mock string")
    );

    send_request(&mut writer, 7, "disconnect", json!({})).await;
    let _disc_resp = read_response(&mut reader, 7).await;

    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_emits_thread_start_and_death_events() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();

    let (client, server_stream) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_stream);
    let server_task =
        tokio::spawn(async move { wire_server::run(server_read, server_write).await });

    let (client_read, client_write) = tokio::io::split(client);
    let mut reader = DapReader::new(client_read);
    let mut writer = DapWriter::new(client_write);

    send_request(&mut writer, 1, "initialize", json!({})).await;
    let init_resp = read_response(&mut reader, 1).await;
    assert!(init_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));
    let initialized = read_next(&mut reader).await;
    assert_eq!(
        initialized.get("event").and_then(|v| v.as_str()),
        Some("initialized")
    );

    send_request(
        &mut writer,
        2,
        "attach",
        json!({
            "host": "127.0.0.1",
            "port": jdwp.addr().port()
        }),
    )
    .await;
    let attach_resp = read_response(&mut reader, 2).await;
    assert!(attach_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

    // Trigger the mock VM to emit thread lifecycle events.
    send_request(&mut writer, 3, "continue", json!({})).await;

    let mut started_thread_id: Option<i64> = None;
    let mut exited_thread_id: Option<i64> = None;
    let mut continue_ok = false;

    tokio::time::timeout(Duration::from_secs(2), async {
        while started_thread_id.is_none() || exited_thread_id.is_none() || !continue_ok {
            let msg = read_next(&mut reader).await;
            match msg.get("type").and_then(|v| v.as_str()) {
                Some("response") => {
                    if msg.get("request_seq").and_then(|v| v.as_i64()) == Some(3) {
                        continue_ok = msg
                            .get("success")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                    }
                }
                Some("event") => {
                    if msg.get("event").and_then(|v| v.as_str()) != Some("thread") {
                        continue;
                    }
                    let reason = msg.pointer("/body/reason").and_then(|v| v.as_str());
                    let thread_id = msg.pointer("/body/threadId").and_then(|v| v.as_i64());
                    match reason {
                        Some("started") => started_thread_id = thread_id,
                        Some("exited") => exited_thread_id = thread_id,
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    })
    .await
    .expect("timed out waiting for thread events");

    assert_eq!(started_thread_id, exited_thread_id);
    assert_eq!(jdwp.vm_resume_calls(), 1);
    assert_eq!(jdwp.thread_resume_calls(), 0);

    send_request(&mut writer, 4, "disconnect", json!({})).await;
    let _disc_resp = read_response(&mut reader, 4).await;

    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_feature_requests_are_guarded_by_jdwp_capabilities() {
    // Mock VM reports all capabilities as `false` by default.
    let jdwp = MockJdwpServer::spawn().await.unwrap();

    let (client, server_stream) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_stream);
    let server_task =
        tokio::spawn(async move { wire_server::run(server_read, server_write).await });

    let (client_read, client_write) = tokio::io::split(client);
    let mut reader = DapReader::new(client_read);
    let mut writer = DapWriter::new(client_write);

    send_request(&mut writer, 1, "initialize", json!({})).await;
    let init_resp = read_response(&mut reader, 1).await;
    assert!(init_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));
    // Initialized event.
    let _initialized = read_next(&mut reader).await;

    send_request(
        &mut writer,
        2,
        "attach",
        json!({
            "host": "127.0.0.1",
            "port": jdwp.addr().port()
        }),
    )
    .await;
    let attach_resp = read_response(&mut reader, 2).await;
    assert!(attach_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

    // Watchpoints / data breakpoints are gated by canWatchField* capabilities.
    send_request(&mut writer, 3, "dataBreakpointInfo", json!({})).await;
    let watch_resp = read_response(&mut reader, 3).await;
    assert!(!watch_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(true));
    let watch_msg = watch_resp
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(watch_msg.contains("canWatchFieldModification"));

    // Hot swap is gated by canRedefineClasses.
    send_request(&mut writer, 4, "redefineClasses", json!({})).await;
    let hot_swap_resp = read_response(&mut reader, 4).await;
    assert!(!hot_swap_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(true));
    let hot_swap_msg = hot_swap_resp
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(hot_swap_msg.contains("canRedefineClasses"));

    // Method return values are gated by canGetMethodReturnValues.
    send_request(&mut writer, 5, "nova/enableMethodReturnValues", json!({})).await;
    let ret_resp = read_response(&mut reader, 5).await;
    assert!(!ret_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(true));
    let ret_msg = ret_resp
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(ret_msg.contains("canGetMethodReturnValues"));

    send_request(&mut writer, 6, "disconnect", json!({})).await;
    let _disc_resp = read_response(&mut reader, 6).await;

    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_evaluate_without_frame_id_returns_friendly_message() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();

    let (client, server_stream) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_stream);
    let server_task = tokio::spawn(async move { wire_server::run(server_read, server_write).await });

    let (client_read, client_write) = tokio::io::split(client);
    let mut reader = DapReader::new(client_read);
    let mut writer = DapWriter::new(client_write);

    send_request(&mut writer, 1, "initialize", json!({})).await;
    let init_resp = read_response(&mut reader, 1).await;
    assert!(init_resp.get("success").and_then(|v| v.as_bool()).unwrap_or(false));
    let initialized = read_next(&mut reader).await;
    assert_eq!(initialized.get("event").and_then(|v| v.as_str()), Some("initialized"));

    send_request(
        &mut writer,
        2,
        "attach",
        json!({
            "host": "127.0.0.1",
            "port": jdwp.addr().port()
        }),
    )
    .await;
    let attach_resp = read_response(&mut reader, 2).await;
    assert!(attach_resp.get("success").and_then(|v| v.as_bool()).unwrap_or(false));

    send_request(&mut writer, 3, "evaluate", json!({ "expression": "x", "context": "hover" })).await;
    let eval_resp = read_response(&mut reader, 3).await;
    assert!(eval_resp.get("success").and_then(|v| v.as_bool()).unwrap_or(false));
    assert_eq!(
        eval_resp.pointer("/body/variablesReference").and_then(|v| v.as_i64()),
        Some(0)
    );
    let result = eval_resp.pointer("/body/result").and_then(|v| v.as_str()).unwrap_or("");
    assert!(result.contains("frameId"));

    send_request(&mut writer, 4, "disconnect", json!({})).await;
    let _disc_resp = read_response(&mut reader, 4).await;

    server_task.await.unwrap().unwrap();
}
