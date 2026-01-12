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
async fn wire_breakpoints_map_to_nearest_executable_line() {
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
            "breakpoints": [ { "line": 4 }, { "line": 2 } ],
        }),
    )
    .await;

    let bp_resp = read_response(&mut reader, 3).await;
    let breakpoints = bp_resp
        .pointer("/body/breakpoints")
        .and_then(|v| v.as_array())
        .unwrap();
    assert_eq!(breakpoints.len(), 2);

    for bp in breakpoints {
        assert!(bp
            .get("verified")
            .and_then(|v| v.as_bool())
            .unwrap_or(false));
        assert_eq!(bp.get("line").and_then(|v| v.as_i64()), Some(3));
    }

    send_request(&mut writer, 4, "disconnect", json!({})).await;
    let _disc_resp = read_response(&mut reader, 4).await;

    server_task.await.unwrap().unwrap();
}
