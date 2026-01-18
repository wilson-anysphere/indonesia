use crate::harness::spawn_wire_server;
use nova_jdwp::wire::{
    mock::{MockEventRequestModifier, MockJdwpServerConfig},
    types::EVENT_KIND_FIELD_MODIFICATION,
};
use serde_json::json;

#[tokio::test]
async fn dap_can_set_data_breakpoints_and_stop_on_field_modification() {
    let (client, server_task) = spawn_wire_server();

    let init = client.initialize_handshake().await;
    assert_eq!(
        init.pointer("/body/supportsDataBreakpoints")
            .and_then(|v| v.as_bool()),
        Some(true)
    );

    let mut config = MockJdwpServerConfig::default();
    config.capabilities[0] = true; // canWatchFieldModification
    config.capabilities[1] = true; // canWatchFieldAccess
    config.field_modification_events = 1;
    // Ensure no other stop events are emitted by default.
    config.breakpoint_events = 0;
    config.step_events = 0;

    let jdwp = client.attach_mock_jdwp_with_config(config).await;

    let thread_id = client.first_thread_id().await;
    let frame_id = client.first_frame_id(thread_id).await;
    let locals_ref = client.first_scope_variables_reference(frame_id).await;

    let vars_resp = client.variables(locals_ref).await;
    let locals = vars_resp
        .pointer("/body/variables")
        .and_then(|v| v.as_array())
        .expect("missing variables array");

    let obj_ref = locals
        .iter()
        .find(|v| v.get("name").and_then(|n| n.as_str()) == Some("obj"))
        .and_then(|v| v.get("variablesReference"))
        .and_then(|v| v.as_i64())
        .expect("locals missing obj.variablesReference");

    let info = client
        .request(
            "dataBreakpointInfo",
            json!({
                "variablesReference": obj_ref,
                "name": "field",
                "frameId": frame_id,
            }),
        )
        .await;
    assert_eq!(info.get("success").and_then(|v| v.as_bool()), Some(true));
    let data_id = info
        .pointer("/body/dataId")
        .and_then(|v| v.as_str())
        .expect("dataBreakpointInfo missing body.dataId")
        .to_string();

    let set = client
        .request(
            "setDataBreakpoints",
            json!({
                "breakpoints": [
                    { "dataId": data_id, "accessType": "write" },
                ],
            }),
        )
        .await;
    assert_eq!(set.get("success").and_then(|v| v.as_bool()), Some(true));
    assert_eq!(
        set.pointer("/body/breakpoints/0/verified")
            .and_then(|v| v.as_bool()),
        Some(true)
    );

    // Validate that the adapter installed a JDWP FieldModification request with FieldOnly +
    // InstanceOnly modifiers.
    let data_id = set
        .pointer("/body/breakpoints/0/id")
        .and_then(|v| v.as_i64())
        .expect("setDataBreakpoints response missing breakpoint id");
    assert!(data_id > 0);

    let requests = jdwp.event_requests().await;
    let watch_req = requests
        .iter()
        .find(|r| r.event_kind == EVENT_KIND_FIELD_MODIFICATION)
        .expect("expected a FieldModification event request to be installed");

    // Extract the field/object identity from the dataId (nova:field:<class>:<field>:<object>).
    let parts: Vec<_> = info
        .pointer("/body/dataId")
        .and_then(|v| v.as_str())
        .unwrap()
        .split(':')
        .collect();
    assert_eq!(parts.get(0).copied(), Some("nova"));
    assert_eq!(parts.get(1).copied(), Some("field"));
    let class_id: u64 = parts.get(2).unwrap().parse().unwrap();
    let field_id: u64 = parts.get(3).unwrap().parse().unwrap();
    let object_id: u64 = parts.get(4).unwrap().parse().unwrap();

    assert_eq!(watch_req.suspend_policy, 1);
    assert!(
        watch_req
            .modifiers
            .contains(&MockEventRequestModifier::FieldOnly { class_id, field_id }),
        "expected FieldOnly modifier in {watch_req:?}"
    );
    assert!(
        watch_req
            .modifiers
            .contains(&MockEventRequestModifier::InstanceOnly { object_id }),
        "expected InstanceOnly modifier in {watch_req:?}"
    );

    // Resume and expect a stop from the mock VM's watchpoint event budget.
    let _ = client.continue_with_thread_id(Some(thread_id)).await;
    let stopped = client.wait_for_stopped_reason("data breakpoint").await;
    assert_eq!(stopped.thread_id, Some(thread_id));
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
async fn dap_rejects_data_breakpoints_when_jdwp_capabilities_missing() {
    let (client, server_task) = spawn_wire_server();
    client.initialize_handshake().await;

    let config = MockJdwpServerConfig::default();
    let _jdwp = client.attach_mock_jdwp_with_config(config).await;

    let resp = client
        .request("setDataBreakpoints", json!({ "breakpoints": [] }))
        .await;
    assert_eq!(resp.get("success").and_then(|v| v.as_bool()), Some(false));
    let message = resp
        .get("message")
        .and_then(|v| v.as_str())
        .expect("error response message");
    assert!(
        message.contains("watchpoints are not supported"),
        "{message}"
    );
    assert!(
        message.contains("canWatchFieldModification=false"),
        "{message}"
    );
    assert!(message.contains("canWatchFieldAccess=false"), "{message}");

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}
