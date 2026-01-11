mod harness;

use std::time::Duration;

use base64::{engine::general_purpose, Engine as _};
use harness::spawn_wire_server;
use nova_dap::object_registry::{OBJECT_HANDLE_BASE, PINNED_SCOPE_REF};
use nova_jdwp::wire::mock::MockJdwpServer;
use serde_json::json;

#[tokio::test]
async fn dap_can_attach_set_breakpoints_and_stop() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();
    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;
    client.attach("127.0.0.1", jdwp.addr().port()).await;

    let bp_resp = client.set_breakpoints("Main.java", &[3]).await;
    let verified = bp_resp
        .pointer("/body/breakpoints/0/verified")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    assert!(verified);
    assert_eq!(jdwp.breakpoint_suspend_policy().await, Some(1));

    let thread_id = client.first_thread_id().await;
    let frame_id = client.first_frame_id(thread_id).await;
    let locals_ref = client.first_scope_variables_reference(frame_id).await;

    let vars_resp = client.variables(locals_ref).await;
    let locals = vars_resp.pointer("/body/variables").and_then(|v| v.as_array()).unwrap();
    assert!(locals.iter().any(|v| v.get("name").and_then(|n| n.as_str()) == Some("x")));

    // Pause should suspend the requested thread and emit a stopped event.
    let (_pause_resp, pause_stopped) = client.pause(Some(thread_id)).await;
    assert_eq!(
        pause_stopped
            .raw
            .pointer("/body/allThreadsStopped")
            .and_then(|v| v.as_bool()),
        Some(false)
    );
    assert_eq!(jdwp.thread_suspend_calls(), 1);
    assert_eq!(jdwp.vm_suspend_calls(), 0);

    // Unknown/unhandled requests should be reported as errors (success: false).
    let bad_resp = client.request("nope", json!({})).await;
    assert_eq!(bad_resp.get("success").and_then(|v| v.as_bool()), Some(false));

    // Continue should emit a continued event and then a stopped event from the mock JDWP VM.
    let (cont_resp, continued) = client.continue_with_thread_id(Some(thread_id)).await;
    assert_eq!(jdwp.thread_resume_calls(), 1);
    assert_eq!(jdwp.vm_resume_calls(), 0);
    assert_eq!(
        cont_resp
            .pointer("/body/allThreadsContinued")
            .and_then(|v| v.as_bool()),
        Some(false)
    );
    assert_eq!(
        continued
            .pointer("/body/allThreadsContinued")
            .and_then(|v| v.as_bool()),
        Some(false)
    );

    let stopped = client.wait_for_stopped_reason("breakpoint").await;
    // The mock JDWP VM uses SuspendPolicy.EVENT_THREAD (only the event thread is suspended).
    assert_eq!(
        stopped
            .raw
            .pointer("/body/allThreadsStopped")
            .and_then(|v| v.as_bool()),
        Some(false)
    );

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_can_hot_swap_a_class() {
    let mut caps = vec![false; 32];
    caps[7] = true; // canRedefineClasses
    let jdwp = MockJdwpServer::spawn_with_capabilities(caps).await.unwrap();

    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;
    client.attach("127.0.0.1", jdwp.addr().port()).await;

    let bytecode = vec![0xCA, 0xFE];
    let bytecode_base64 = general_purpose::STANDARD.encode(&bytecode);
    let hot_swap_resp = client
        .request(
            "nova/hotSwap",
            json!({
                "classes": [{
                    "className": "Main",
                    "bytecodeBase64": bytecode_base64,
                }]
            }),
        )
        .await;

    assert_eq!(
        hot_swap_resp.get("success").and_then(|v| v.as_bool()),
        Some(true)
    );
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

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_hot_swap_reports_schema_change() {
    let mut caps = vec![false; 32];
    caps[7] = true; // canRedefineClasses
    let jdwp = MockJdwpServer::spawn_with_capabilities(caps).await.unwrap();
    jdwp.set_redefine_classes_error_code(62); // SCHEMA_CHANGE_NOT_IMPLEMENTED

    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;
    client.attach("127.0.0.1", jdwp.addr().port()).await;

    let bytecode_base64 = general_purpose::STANDARD.encode([0u8; 4]);
    let hot_swap_resp = client
        .request(
            "nova/hotSwap",
            json!({
                "classes": [{
                    "className": "Main",
                    "bytecodeBase64": bytecode_base64,
                }]
            }),
        )
        .await;

    assert_eq!(
        hot_swap_resp.get("success").and_then(|v| v.as_bool()),
        Some(true)
    );
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

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_wire_handle_tables_are_stable_within_stop_and_invalidated_on_resume() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();
    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;
    client.attach("127.0.0.1", jdwp.addr().port()).await;

    let bp_resp = client.set_breakpoints("Main.java", &[3]).await;
    assert!(bp_resp
        .pointer("/body/breakpoints/0/verified")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));
    assert_eq!(jdwp.breakpoint_suspend_policy().await, Some(1));

    let thread_id = client.first_thread_id().await;

    // Continue to generate an initial stop.
    client.continue_with_thread_id(Some(thread_id)).await;
    let stopped = client.wait_for_stopped_reason("breakpoint").await;
    assert_eq!(
        stopped
            .raw
            .pointer("/body/allThreadsStopped")
            .and_then(|v| v.as_bool()),
        Some(false)
    );

    // Repeated stackTrace calls should return stable frame ids.
    let stack_a = client.request("stackTrace", json!({ "threadId": thread_id })).await;
    let frame_id_a = stack_a
        .pointer("/body/stackFrames/0/id")
        .and_then(|v| v.as_i64())
        .unwrap();
    let stack_b = client.request("stackTrace", json!({ "threadId": thread_id })).await;
    let frame_id_b = stack_b
        .pointer("/body/stackFrames/0/id")
        .and_then(|v| v.as_i64())
        .unwrap();
    assert_eq!(frame_id_a, frame_id_b);

    // And repeated scopes calls should return stable locals handles.
    let scopes_a = client.request("scopes", json!({ "frameId": frame_id_a })).await;
    let locals_ref_a = scopes_a
        .pointer("/body/scopes/0/variablesReference")
        .and_then(|v| v.as_i64())
        .unwrap();
    let scopes_b = client.request("scopes", json!({ "frameId": frame_id_a })).await;
    let locals_ref_b = scopes_b
        .pointer("/body/scopes/0/variablesReference")
        .and_then(|v| v.as_i64())
        .unwrap();
    assert_eq!(locals_ref_a, locals_ref_b);

    // Resume; the next stop should allocate fresh handles (stale ids must not alias).
    client.continue_with_thread_id(Some(thread_id)).await;
    let _ = client.wait_for_stopped_reason("breakpoint").await;

    let stack_after = client.request("stackTrace", json!({ "threadId": thread_id })).await;
    let frame_id_after = stack_after
        .pointer("/body/stackFrames/0/id")
        .and_then(|v| v.as_i64())
        .unwrap();
    assert_ne!(frame_id_a, frame_id_after);

    let scopes_after = client.request("scopes", json!({ "frameId": frame_id_after })).await;
    let locals_ref_after = scopes_after
        .pointer("/body/scopes/0/variablesReference")
        .and_then(|v| v.as_i64())
        .unwrap();
    assert_ne!(locals_ref_a, locals_ref_after);

    // Old frame ids should be rejected rather than resolving to a different frame.
    let stale_scopes = client.request("scopes", json!({ "frameId": frame_id_a })).await;
    assert_eq!(stale_scopes.get("success").and_then(|v| v.as_bool()), Some(false));

    // Old variables references should return empty results.
    let stale_vars = client
        .request("variables", json!({ "variablesReference": locals_ref_a }))
        .await;
    let vars = stale_vars
        .pointer("/body/variables")
        .and_then(|v| v.as_array())
        .unwrap();
    assert!(vars.is_empty());

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_object_handles_are_stable_across_stops_and_pinning_exposes_them_in_a_scope() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();
    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;
    client.attach("127.0.0.1", jdwp.addr().port()).await;

    let bp_resp = client.set_breakpoints("Main.java", &[3]).await;
    assert!(bp_resp
        .pointer("/body/breakpoints/0/verified")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

    let thread_id = client.first_thread_id().await;

    // First stop.
    client.continue_with_thread_id(Some(thread_id)).await;
    let _ = client.wait_for_stopped_reason("breakpoint").await;

    let frame_id = client.first_frame_id(thread_id).await;
    let scopes_resp = client.request("scopes", json!({ "frameId": frame_id })).await;
    let locals_ref = scopes_resp
        .pointer("/body/scopes/0/variablesReference")
        .and_then(|v| v.as_i64())
        .unwrap();

    let vars_resp = client
        .request("variables", json!({ "variablesReference": locals_ref }))
        .await;
    let locals = vars_resp.pointer("/body/variables").and_then(|v| v.as_array()).unwrap();
    let obj_ref = locals
        .iter()
        .find(|v| v.get("name").and_then(|n| n.as_str()) == Some("obj"))
        .and_then(|v| v.get("variablesReference"))
        .and_then(|v| v.as_i64())
        .unwrap();
    assert!(obj_ref > OBJECT_HANDLE_BASE);

    // Not pinned: object handles should remain stable across resumes as long as the
    // underlying object is still alive.
    client.continue_with_thread_id(Some(thread_id)).await;
    let _ = client.wait_for_stopped_reason("breakpoint").await;

    let stale_obj_vars = client
        .request("variables", json!({ "variablesReference": obj_ref }))
        .await;
    let stale = stale_obj_vars
        .pointer("/body/variables")
        .and_then(|v| v.as_array())
        .unwrap();
    assert!(!stale.is_empty());

    // Pin a fresh object handle.
    let frame_id = client.first_frame_id(thread_id).await;
    let scopes_resp = client.request("scopes", json!({ "frameId": frame_id })).await;
    let locals_ref = scopes_resp
        .pointer("/body/scopes/0/variablesReference")
        .and_then(|v| v.as_i64())
        .unwrap();
    let pinned_ref = scopes_resp
        .pointer("/body/scopes/1/variablesReference")
        .and_then(|v| v.as_i64())
        .unwrap();
    assert_eq!(pinned_ref, PINNED_SCOPE_REF);

    let vars_resp = client
        .request("variables", json!({ "variablesReference": locals_ref }))
        .await;
    let locals = vars_resp.pointer("/body/variables").and_then(|v| v.as_array()).unwrap();
    let obj_ref = locals
        .iter()
        .find(|v| v.get("name").and_then(|n| n.as_str()) == Some("obj"))
        .and_then(|v| v.get("variablesReference"))
        .and_then(|v| v.as_i64())
        .unwrap();

    let pin_resp = client
        .request(
            "nova/pinObject",
            json!({ "variablesReference": obj_ref, "pinned": true }),
        )
        .await;
    assert_eq!(pin_resp.pointer("/body/pinned").and_then(|v| v.as_bool()), Some(true));

    // Resume again; pinned handle must survive.
    client.continue_with_thread_id(Some(thread_id)).await;
    let _ = client.wait_for_stopped_reason("breakpoint").await;

    let pinned_vars_resp = client
        .request("variables", json!({ "variablesReference": PINNED_SCOPE_REF }))
        .await;
    let pinned_vars = pinned_vars_resp
        .pointer("/body/variables")
        .and_then(|v| v.as_array())
        .unwrap();
    assert!(pinned_vars
        .iter()
        .any(|v| v.get("variablesReference").and_then(|v| v.as_i64()) == Some(obj_ref)));

    let obj_vars = client
        .request("variables", json!({ "variablesReference": obj_ref }))
        .await;
    let fields = obj_vars
        .pointer("/body/variables")
        .and_then(|v| v.as_array())
        .unwrap();
    assert!(!fields.is_empty());

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_step_stop_uses_event_thread_suspend_policy() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();
    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;
    client.attach("127.0.0.1", jdwp.addr().port()).await;

    let thread_id = client.first_thread_id().await;
    client.next(thread_id).await;
    let stopped = client.wait_for_stopped_reason("step").await;

    assert_eq!(jdwp.thread_resume_calls(), 1);
    assert_eq!(jdwp.vm_resume_calls(), 0);
    assert_eq!(jdwp.step_suspend_policy().await, Some(1));
    assert_eq!(
        stopped
            .raw
            .pointer("/body/allThreadsStopped")
            .and_then(|v| v.as_bool()),
        Some(false)
    );

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_can_expand_object_fields_and_pin_objects() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();
    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;
    client.attach("127.0.0.1", jdwp.addr().port()).await;

    let thread_id = client.first_thread_id().await;
    let frame_id = client.first_frame_id(thread_id).await;

    let scopes_resp = client.request("scopes", json!({ "frameId": frame_id })).await;
    let locals_ref = scopes_resp
        .pointer("/body/scopes/0/variablesReference")
        .and_then(|v| v.as_i64())
        .unwrap();
    let pinned_ref = scopes_resp
        .pointer("/body/scopes/1/variablesReference")
        .and_then(|v| v.as_i64())
        .unwrap();
    assert_eq!(pinned_ref, PINNED_SCOPE_REF);

    let vars_resp = client
        .request("variables", json!({ "variablesReference": locals_ref }))
        .await;
    let locals = vars_resp.pointer("/body/variables").and_then(|v| v.as_array()).unwrap();

    let obj = locals
        .iter()
        .find(|v| v.get("name").and_then(|n| n.as_str()) == Some("obj"))
        .expect("expected locals to contain obj");
    let obj_ref = obj.get("variablesReference").and_then(|v| v.as_i64()).unwrap();
    assert!(
        obj_ref > OBJECT_HANDLE_BASE,
        "expected stable object handle variablesReference"
    );
    assert!(obj
        .get("value")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .contains('@'));

    let fields_resp = client.request("variables", json!({ "variablesReference": obj_ref })).await;
    let fields = fields_resp.pointer("/body/variables").and_then(|v| v.as_array()).unwrap();
    let field = fields
        .iter()
        .find(|v| v.get("name").and_then(|n| n.as_str()) == Some("field"))
        .expect("expected object to contain field");
    assert_eq!(field.get("value").and_then(|v| v.as_str()), Some("7"));
    assert_eq!(field.get("type").and_then(|v| v.as_str()), Some("int"));

    // Pin the object.
    let pin_resp = client
        .request(
            "nova/pinObject",
            json!({ "variablesReference": obj_ref, "pinned": true }),
        )
        .await;
    assert_eq!(pin_resp.pointer("/body/pinned").and_then(|v| v.as_bool()), Some(true));

    // Pinned objects are visible under the synthetic scope.
    let pinned_vars_resp = client
        .request("variables", json!({ "variablesReference": PINNED_SCOPE_REF }))
        .await;
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
    let unpin_resp = client
        .request(
            "nova/pinObject",
            json!({ "variablesReference": obj_ref, "pinned": false }),
        )
        .await;
    assert_eq!(
        unpin_resp.pointer("/body/pinned").and_then(|v| v.as_bool()),
        Some(false)
    );

    let pinned_empty_resp = client
        .request("variables", json!({ "variablesReference": PINNED_SCOPE_REF }))
        .await;
    let pinned_empty = pinned_empty_resp
        .pointer("/body/variables")
        .and_then(|v| v.as_array())
        .unwrap();
    assert!(pinned_empty.is_empty());

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_exception_info_includes_type_name() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();
    let (client, server_task) = spawn_wire_server();

    let init_resp = client.initialize_handshake().await;
    assert_eq!(
        init_resp
            .pointer("/body/supportsExceptionInfoRequest")
            .and_then(|v| v.as_bool()),
        Some(true)
    );

    client.attach("127.0.0.1", jdwp.addr().port()).await;

    let exc_bp_resp = client
        .request("setExceptionBreakpoints", json!({ "filters": ["all"] }))
        .await;
    assert_eq!(exc_bp_resp.get("success").and_then(|v| v.as_bool()), Some(true));

    let thread_id = client.first_thread_id().await;

    client.continue_with_thread_id(Some(thread_id)).await;
    let stopped = client.wait_for_stopped_reason("exception").await;
    assert_eq!(stopped.reason.as_deref(), Some("exception"));

    let exc_info = client
        .request("exceptionInfo", json!({ "threadId": thread_id }))
        .await;
    assert_eq!(exc_info.get("success").and_then(|v| v.as_bool()), Some(true));
    assert_eq!(
        exc_info.pointer("/body/exceptionId").and_then(|v| v.as_str()),
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
        exc_info.pointer("/body/description").and_then(|v| v.as_str()),
        Some("mock string")
    );
    assert_eq!(
        exc_info
            .pointer("/body/details/message")
            .and_then(|v| v.as_str()),
        Some("mock string")
    );

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_emits_thread_start_and_death_events() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();
    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;
    client.attach("127.0.0.1", jdwp.addr().port()).await;

    // Trigger the mock VM to emit thread lifecycle events.
    client.continue_().await;

    let started = client
        .wait_for_event_matching(
            "thread(started)",
            Duration::from_secs(2),
            |msg| {
                msg.get("type").and_then(|v| v.as_str()) == Some("event")
                    && msg.get("event").and_then(|v| v.as_str()) == Some("thread")
                    && msg.pointer("/body/reason").and_then(|v| v.as_str()) == Some("started")
            },
        )
        .await;
    let exited = client
        .wait_for_event_matching(
            "thread(exited)",
            Duration::from_secs(2),
            |msg| {
                msg.get("type").and_then(|v| v.as_str()) == Some("event")
                    && msg.get("event").and_then(|v| v.as_str()) == Some("thread")
                    && msg.pointer("/body/reason").and_then(|v| v.as_str()) == Some("exited")
            },
        )
        .await;

    let started_thread_id = started.pointer("/body/threadId").and_then(|v| v.as_i64());
    let exited_thread_id = exited.pointer("/body/threadId").and_then(|v| v.as_i64());
    assert_eq!(started_thread_id, exited_thread_id);
    assert_eq!(jdwp.vm_resume_calls(), 1);
    assert_eq!(jdwp.thread_resume_calls(), 0);

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_feature_requests_are_guarded_by_jdwp_capabilities() {
    // Mock VM reports all capabilities as `false` by default.
    let jdwp = MockJdwpServer::spawn().await.unwrap();
    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;
    client.attach("127.0.0.1", jdwp.addr().port()).await;

    // Watchpoints / data breakpoints are gated by canWatchField* capabilities.
    let watch_resp = client.request("dataBreakpointInfo", json!({})).await;
    assert_eq!(watch_resp.get("success").and_then(|v| v.as_bool()), Some(false));
    let watch_msg = watch_resp.get("message").and_then(|v| v.as_str()).unwrap_or("");
    assert!(watch_msg.contains("canWatchFieldModification"));

    // Hot swap is gated by canRedefineClasses.
    let hot_swap_resp = client.request("redefineClasses", json!({})).await;
    assert_eq!(
        hot_swap_resp.get("success").and_then(|v| v.as_bool()),
        Some(false)
    );
    let hot_swap_msg = hot_swap_resp
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(hot_swap_msg.contains("canRedefineClasses"));

    // Method return values are gated by canGetMethodReturnValues.
    let ret_resp = client
        .request("nova/enableMethodReturnValues", json!({}))
        .await;
    assert_eq!(ret_resp.get("success").and_then(|v| v.as_bool()), Some(false));
    let ret_msg = ret_resp.get("message").and_then(|v| v.as_str()).unwrap_or("");
    assert!(ret_msg.contains("canGetMethodReturnValues"));

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_evaluate_without_frame_id_returns_friendly_message() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();
    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;
    client.attach("127.0.0.1", jdwp.addr().port()).await;

    let eval_resp = client
        .request(
            "evaluate",
            json!({
                "expression": "x",
                "context": "hover",
            }),
        )
        .await;
    assert_eq!(eval_resp.get("success").and_then(|v| v.as_bool()), Some(true));
    assert_eq!(
        eval_resp
            .pointer("/body/variablesReference")
            .and_then(|v| v.as_i64()),
        Some(0)
    );
    let result = eval_resp
        .pointer("/body/result")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(result.contains("frameId"));

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_emits_output_for_expression_value_on_step() {
    // Enable `canGetMethodReturnValues` so the adapter can install a MethodExitWithReturnValue
    // request during stepping.
    let mut caps = vec![false; 32];
    caps[22] = true;
    let jdwp = MockJdwpServer::spawn_with_capabilities(caps).await.unwrap();

    let (client, server_stream) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_stream);
    let server_task = tokio::spawn(async move { wire_server::run(server_read, server_write).await });

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

    // Issue a step-over request and expect:
    //  - an `output` event for the expression value
    //  - a `stopped` event (reason=step)
    send_request(&mut writer, 4, "next", json!({ "threadId": thread_id })).await;

    let mut resp = None;
    let mut output_evt = None;
    let mut stopped_evt = None;
    for _ in 0..50 {
        let msg = read_next(&mut reader).await;
        if msg.get("type").and_then(|v| v.as_str()) == Some("response")
            && msg.get("request_seq").and_then(|v| v.as_i64()) == Some(4)
        {
            resp = Some(msg);
        } else if msg.get("type").and_then(|v| v.as_str()) == Some("event")
            && msg.get("event").and_then(|v| v.as_str()) == Some("output")
        {
            output_evt = Some(msg);
        } else if msg.get("type").and_then(|v| v.as_str()) == Some("event")
            && msg.get("event").and_then(|v| v.as_str()) == Some("stopped")
        {
            stopped_evt = Some(msg);
        }

        if resp.is_some() && output_evt.is_some() && stopped_evt.is_some() {
            break;
        }
    }

    let resp = resp.expect("expected next response");
    assert!(resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

    let output_evt = output_evt.expect("expected output event");
    assert!(output_evt
        .pointer("/body/output")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .contains("Expression value:"));

    let stopped_evt = stopped_evt.expect("expected stopped event");
    assert_eq!(
        stopped_evt.pointer("/body/reason").and_then(|v| v.as_str()),
        Some("step")
    );

    // The output should be emitted before the stop notification (mirrors legacy UX).
    let output_seq = output_evt.get("seq").and_then(|v| v.as_i64()).unwrap_or(0);
    let stopped_seq = stopped_evt.get("seq").and_then(|v| v.as_i64()).unwrap_or(0);
    assert!(output_seq < stopped_seq);

    send_request(&mut writer, 5, "disconnect", json!({})).await;
    let _disc_resp = read_response(&mut reader, 5).await;

    server_task.await.unwrap().unwrap();
}
