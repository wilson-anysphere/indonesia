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

#[tokio::test]
async fn dap_hit_count_breakpoints_use_jdwp_count_modifier() {
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

    let initialized = read_event(&mut reader, "initialized").await;
    assert_eq!(
        initialized.get("event").and_then(|v| v.as_str()),
        Some("initialized")
    );

    send_request(
        &mut writer,
        3,
        "setBreakpoints",
        json!({
            "source": { "path": "Main.java" },
            "breakpoints": [ { "line": 3, "hitCondition": "3" } ],
        }),
    )
    .await;
    let bp_resp = read_response(&mut reader, 3).await;
    let verified = bp_resp
        .pointer("/body/breakpoints/0/verified")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    assert!(verified);

    assert_eq!(jdwp.breakpoint_count_modifier().await, Some(3));

    send_request(&mut writer, 4, "disconnect", json!({})).await;
    let _disc_resp = read_response(&mut reader, 4).await;

    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_logpoints_emit_output_without_stopping() {
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

    let initialized = read_event(&mut reader, "initialized").await;
    assert_eq!(
        initialized.get("event").and_then(|v| v.as_str()),
        Some("initialized")
    );

    send_request(
        &mut writer,
        3,
        "setBreakpoints",
        json!({
            "source": { "path": "Main.java" },
            "breakpoints": [ { "line": 3, "logMessage": "hello {x}" } ],
        }),
    )
    .await;
    let bp_resp = read_response(&mut reader, 3).await;
    let verified = bp_resp
        .pointer("/body/breakpoints/0/verified")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    assert!(verified);
    assert_eq!(jdwp.breakpoint_suspend_policy().await, Some(0));

    send_request(&mut writer, 4, "threads", json!({})).await;
    let threads_resp = read_response(&mut reader, 4).await;
    let thread_id = threads_resp
        .pointer("/body/threads/0/id")
        .and_then(|v| v.as_i64())
        .unwrap();

    // Continue and expect an output event (but no stopped event) for the logpoint.
    send_request(&mut writer, 5, "continue", json!({ "threadId": thread_id })).await;

    let mut saw_continue_response = false;
    let mut saw_output = false;
    for _ in 0..50 {
        let msg = read_next(&mut reader).await;
        if msg.get("type").and_then(|v| v.as_str()) == Some("response")
            && msg.get("request_seq").and_then(|v| v.as_i64()) == Some(5)
        {
            saw_continue_response = true;
        }

        if msg.get("type").and_then(|v| v.as_str()) == Some("event") {
            if msg.get("event").and_then(|v| v.as_str()) == Some("stopped") {
                panic!("logpoint should not emit stopped event: {msg:?}");
            }
            if msg.get("event").and_then(|v| v.as_str()) == Some("output") {
                let output = msg
                    .pointer("/body/output")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if output.contains("hello") {
                    saw_output = true;
                }
            }
        }

        if saw_continue_response && saw_output {
            break;
        }
    }

    assert!(saw_continue_response, "missing continue response");
    assert!(saw_output, "missing output event for logpoint");

    send_request(&mut writer, 6, "disconnect", json!({})).await;
    let _disc_resp = read_response(&mut reader, 6).await;

    server_task.await.unwrap().unwrap();
}
