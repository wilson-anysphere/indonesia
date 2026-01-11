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
async fn dap_can_stop_on_uncaught_exceptions() {
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
            "port": jdwp.addr().port()
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

    send_request(&mut writer, 3, "threads", json!({})).await;
    let threads_resp = read_response(&mut reader, 3).await;
    let thread_id = threads_resp
        .pointer("/body/threads/0/id")
        .and_then(|v| v.as_i64())
        .unwrap();

    send_request(
        &mut writer,
        4,
        "setExceptionBreakpoints",
        json!({ "filters": ["uncaught"] }),
    )
    .await;
    let set_exc_resp = read_response(&mut reader, 4).await;
    assert!(set_exc_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

    let exc_req = jdwp
        .exception_request()
        .await
        .expect("expected exception request to be configured");
    assert!(!exc_req.caught);
    assert!(exc_req.uncaught);

    send_request(&mut writer, 5, "continue", json!({ "threadId": thread_id })).await;
    let cont_resp = read_response(&mut reader, 5).await;
    assert!(cont_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

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
        Some("unhandled")
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
