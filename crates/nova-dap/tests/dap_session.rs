use serde_json::{json, Value};

use nova_dap::dap_tokio::{DapReader, DapWriter};
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
        Some(true)
    );

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
    assert_eq!(
        cont_resp
            .pointer("/body/allThreadsContinued")
            .and_then(|v| v.as_bool()),
        Some(true)
    );

    let continued = continued.expect("expected continued event");
    assert_eq!(
        continued
            .pointer("/body/allThreadsContinued")
            .and_then(|v| v.as_bool()),
        Some(true)
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
