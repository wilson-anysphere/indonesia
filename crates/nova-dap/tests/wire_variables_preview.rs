use serde_json::{json, Value};

use nova_dap::dap_tokio::{DapReader, DapWriter};
use nova_dap::{wire_server, PINNED_SCOPE_REF};
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

async fn read_event(
    reader: &mut DapReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    event: &str,
) -> Value {
    for _ in 0..50 {
        let msg = read_next(reader).await;
        if msg.get("type").and_then(|v| v.as_str()) == Some("event")
            && msg.get("event").and_then(|v| v.as_str()) == Some(event)
        {
            return msg;
        }
    }
    panic!("did not receive event {event}");
}

fn find_var<'a>(vars: &'a [Value], name: &str) -> &'a Value {
    vars.iter()
        .find(|v| v.get("name").and_then(|n| n.as_str()) == Some(name))
        .unwrap_or_else(|| panic!("missing variable {name}"))
}

#[tokio::test]
async fn wire_variables_have_richer_previews_and_support_pinning() {
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
    let initialized = read_event(&mut reader, "initialized").await;
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
            "port": jdwp.addr().port(),
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
    let scopes = scopes_resp
        .pointer("/body/scopes")
        .and_then(|v| v.as_array())
        .unwrap();
    assert_eq!(scopes.len(), 3);
    assert_eq!(
        scopes[0].get("name").and_then(|v| v.as_str()),
        Some("Locals")
    );
    assert_eq!(
        scopes[1].get("name").and_then(|v| v.as_str()),
        Some("Pinned Objects")
    );
    assert_eq!(
        scopes[2].get("name").and_then(|v| v.as_str()),
        Some("Static")
    );
    assert_eq!(
        scopes[1]["variablesReference"].as_i64().unwrap(),
        PINNED_SCOPE_REF
    );

    let locals_ref = scopes[0]["variablesReference"].as_i64().unwrap();

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

    let string_var = find_var(locals, "s");
    let string_value = string_var.get("value").and_then(|v| v.as_str()).unwrap();
    assert!(string_value.starts_with("\"mock string\""));
    assert_eq!(
        string_var.get("type").and_then(|v| v.as_str()),
        Some("java.lang.String")
    );

    let array_var = find_var(locals, "arr");
    let array_ref = array_var
        .get("variablesReference")
        .and_then(|v| v.as_i64())
        .unwrap();
    assert!(array_ref > 1000);
    assert_eq!(
        array_var.get("type").and_then(|v| v.as_str()),
        Some("int[]")
    );

    // Handles should be stable across multiple variables() calls.
    send_request(
        &mut writer,
        8,
        "variables",
        json!({ "variablesReference": locals_ref }),
    )
    .await;
    let vars_resp2 = read_response(&mut reader, 8).await;
    let locals2 = vars_resp2
        .pointer("/body/variables")
        .and_then(|v| v.as_array())
        .unwrap();
    let array_ref2 = find_var(locals2, "arr")
        .get("variablesReference")
        .and_then(|v| v.as_i64())
        .unwrap();
    assert_eq!(array_ref2, array_ref);

    // Array children should support paging via start/count.
    send_request(
        &mut writer,
        9,
        "variables",
        json!({ "variablesReference": array_ref, "start": 1, "count": 2 }),
    )
    .await;
    let array_vars = read_response(&mut reader, 9).await;
    let elements = array_vars
        .pointer("/body/variables")
        .and_then(|v| v.as_array())
        .unwrap();
    assert_eq!(elements.len(), 2);
    assert_eq!(
        elements[0].get("name").and_then(|v| v.as_str()),
        Some("[1]")
    );
    assert_eq!(elements[0].get("value").and_then(|v| v.as_str()), Some("1"));
    assert_eq!(
        elements[0].get("evaluateName").and_then(|v| v.as_str()),
        Some("arr[1]")
    );
    assert_eq!(
        elements[1].get("name").and_then(|v| v.as_str()),
        Some("[2]")
    );
    assert_eq!(elements[1].get("value").and_then(|v| v.as_str()), Some("2"));

    // Pin the array object.
    assert!(jdwp.pinned_object_ids().await.is_empty());
    send_request(
        &mut writer,
        10,
        "nova/pinObject",
        json!({ "variablesReference": array_ref, "pinned": true }),
    )
    .await;
    let pin_resp = read_response(&mut reader, 10).await;
    assert!(pin_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));
    assert_eq!(
        pin_resp.pointer("/body/pinned").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(jdwp.pinned_object_ids().await.len(), 1);

    // Pinned scope should now include the array handle.
    send_request(
        &mut writer,
        11,
        "variables",
        json!({ "variablesReference": PINNED_SCOPE_REF }),
    )
    .await;
    let pinned_vars_resp = read_response(&mut reader, 11).await;
    let pinned_vars = pinned_vars_resp
        .pointer("/body/variables")
        .and_then(|v| v.as_array())
        .unwrap();
    assert_eq!(pinned_vars.len(), 1);
    assert_eq!(
        pinned_vars[0]["variablesReference"].as_i64().unwrap(),
        array_ref
    );

    // Unpin and ensure EnableCollection fires.
    send_request(
        &mut writer,
        12,
        "nova/pinObject",
        json!({ "variablesReference": array_ref, "pinned": false }),
    )
    .await;
    let unpin_resp = read_response(&mut reader, 12).await;
    assert_eq!(
        unpin_resp.pointer("/body/pinned").and_then(|v| v.as_bool()),
        Some(false)
    );
    assert!(jdwp.pinned_object_ids().await.is_empty());

    send_request(
        &mut writer,
        13,
        "variables",
        json!({ "variablesReference": PINNED_SCOPE_REF }),
    )
    .await;
    let pinned_vars_resp2 = read_response(&mut reader, 13).await;
    let pinned_vars2 = pinned_vars_resp2
        .pointer("/body/variables")
        .and_then(|v| v.as_array())
        .unwrap();
    assert!(pinned_vars2.is_empty());

    // Object handles should remain stable across stops.
    send_request(
        &mut writer,
        14,
        "continue",
        json!({ "threadId": thread_id }),
    )
    .await;
    let mut continue_resp = None;
    let mut stopped_evt = None;
    for _ in 0..50 {
        let msg = read_next(&mut reader).await;
        if msg.get("type").and_then(|v| v.as_str()) == Some("response")
            && msg.get("request_seq").and_then(|v| v.as_i64()) == Some(14)
        {
            continue_resp = Some(msg.clone());
        }
        if msg.get("type").and_then(|v| v.as_str()) == Some("event")
            && msg.get("event").and_then(|v| v.as_str()) == Some("stopped")
        {
            stopped_evt = Some(msg.clone());
        }
        if continue_resp.is_some() && stopped_evt.is_some() {
            break;
        }
    }
    let continue_resp = continue_resp.expect("missing continue response");
    assert!(continue_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));
    let stopped_evt = stopped_evt.expect("missing stopped event");
    assert_eq!(
        stopped_evt.pointer("/body/reason").and_then(|v| v.as_str()),
        Some("breakpoint")
    );

    send_request(
        &mut writer,
        15,
        "stackTrace",
        json!({ "threadId": thread_id }),
    )
    .await;
    let stack_resp2 = read_response(&mut reader, 15).await;
    let frame_id2 = stack_resp2
        .pointer("/body/stackFrames/0/id")
        .and_then(|v| v.as_i64())
        .unwrap();
    send_request(&mut writer, 16, "scopes", json!({ "frameId": frame_id2 })).await;
    let scopes_resp2 = read_response(&mut reader, 16).await;
    let locals_ref2 = scopes_resp2
        .pointer("/body/scopes/0/variablesReference")
        .and_then(|v| v.as_i64())
        .unwrap();
    send_request(
        &mut writer,
        17,
        "variables",
        json!({ "variablesReference": locals_ref2 }),
    )
    .await;
    let vars_resp3 = read_response(&mut reader, 17).await;
    let locals3 = vars_resp3
        .pointer("/body/variables")
        .and_then(|v| v.as_array())
        .unwrap();
    let array_ref3 = find_var(locals3, "arr")
        .get("variablesReference")
        .and_then(|v| v.as_i64())
        .unwrap();
    assert_eq!(array_ref3, array_ref);

    send_request(&mut writer, 18, "disconnect", json!({})).await;
    let _disc_resp = read_response(&mut reader, 18).await;
    server_task.await.unwrap().unwrap();
}
