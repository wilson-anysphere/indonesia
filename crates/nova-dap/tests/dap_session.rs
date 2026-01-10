use serde_json::{json, Value};

use nova_dap::dap_tokio::{DapReader, DapWriter};
use nova_dap::wire_server;
use nova_jdwp::wire::mock::MockJdwpServer;

async fn send_request(writer: &mut DapWriter<tokio::io::WriteHalf<tokio::io::DuplexStream>>, seq: i64, command: &str, arguments: Value) {
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
    let server_task = tokio::spawn(async move { wire_server::run(server_read, server_write).await });

    let (client_read, client_write) = tokio::io::split(client);
    let mut reader = DapReader::new(client_read);
    let mut writer = DapWriter::new(client_write);

    send_request(&mut writer, 1, "initialize", json!({})).await;
    let init_resp = read_response(&mut reader, 1).await;
    assert!(init_resp.get("success").and_then(|v| v.as_bool()).unwrap_or(false));
    // Initialized event.
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

    send_request(&mut writer, 5, "stackTrace", json!({ "threadId": thread_id })).await;
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

    send_request(&mut writer, 7, "variables", json!({ "variablesReference": locals_ref })).await;
    let vars_resp = read_response(&mut reader, 7).await;
    let locals = vars_resp.pointer("/body/variables").and_then(|v| v.as_array()).unwrap();
    assert!(locals.iter().any(|v| v.get("name").and_then(|n| n.as_str()) == Some("x")));

    // Continue and expect a stopped event from the mock JDWP VM.
    send_request(&mut writer, 8, "continue", json!({ "threadId": thread_id })).await;
    let cont_resp = read_response(&mut reader, 8).await;
    assert!(cont_resp.get("success").and_then(|v| v.as_bool()).unwrap_or(false));

    // stopped event can race with continue response; scan a bit.
    let mut stopped = None;
    for _ in 0..20 {
        let msg = read_next(&mut reader).await;
        if msg.get("type").and_then(|v| v.as_str()) == Some("event")
            && msg.get("event").and_then(|v| v.as_str()) == Some("stopped")
        {
            stopped = Some(msg);
            break;
        }
    }
    let stopped = stopped.expect("expected stopped event");
    assert_eq!(
        stopped.pointer("/body/reason").and_then(|v| v.as_str()),
        Some("breakpoint")
    );

    send_request(&mut writer, 9, "disconnect", json!({})).await;
    let _disc_resp = read_response(&mut reader, 9).await;

    server_task.await.unwrap().unwrap();
}
