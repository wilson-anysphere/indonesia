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

async fn read_until_response(
    reader: &mut DapReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    request_seq: i64,
) -> (Value, Vec<Value>) {
    let mut other = Vec::new();
    for _ in 0..200 {
        let msg = read_next(reader).await;
        if msg.get("type").and_then(|v| v.as_str()) == Some("response")
            && msg.get("request_seq").and_then(|v| v.as_i64()) == Some(request_seq)
        {
            return (msg, other);
        }
        other.push(msg);
    }
    panic!("did not receive response for seq {request_seq}");
}

fn is_event(msg: &Value, name: &str) -> bool {
    msg.get("type").and_then(|v| v.as_str()) == Some("event")
        && msg.get("event").and_then(|v| v.as_str()) == Some(name)
}

#[tokio::test]
async fn attach_disconnect_detaches_via_dispose() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();

    let (client, server_stream) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_stream);
    let server_task = tokio::spawn(async move { wire_server::run(server_read, server_write).await });

    let (client_read, client_write) = tokio::io::split(client);
    let mut reader = DapReader::new(client_read);
    let mut writer = DapWriter::new(client_write);

    send_request(&mut writer, 1, "initialize", json!({})).await;
    let (init_resp, _) = read_until_response(&mut reader, 1).await;
    assert!(init_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

    let initialized_evt = read_next(&mut reader).await;
    assert!(is_event(&initialized_evt, "initialized"));

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
    let (attach_resp, _) = read_until_response(&mut reader, 2).await;
    assert!(attach_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

    send_request(
        &mut writer,
        3,
        "disconnect",
        json!({ "terminateDebuggee": false }),
    )
    .await;
    let (disc_resp, disc_messages) = read_until_response(&mut reader, 3).await;
    assert!(disc_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

    // `terminated` can arrive before or after the disconnect response depending on scheduling.
    let mut saw_terminated = disc_messages.iter().any(|msg| is_event(msg, "terminated"));
    for _ in 0..50 {
        if saw_terminated {
            break;
        }
        let msg = read_next(&mut reader).await;
        saw_terminated = is_event(&msg, "terminated");
    }
    assert!(saw_terminated, "expected terminated event after disconnect");

    server_task.await.unwrap().unwrap();

    assert_eq!(jdwp.virtual_machine_dispose_calls(), 1);
}

#[tokio::test]
async fn attach_terminate_uses_virtual_machine_exit() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();

    let (client, server_stream) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_stream);
    let server_task = tokio::spawn(async move { wire_server::run(server_read, server_write).await });

    let (client_read, client_write) = tokio::io::split(client);
    let mut reader = DapReader::new(client_read);
    let mut writer = DapWriter::new(client_write);

    send_request(&mut writer, 1, "initialize", json!({})).await;
    let (init_resp, _) = read_until_response(&mut reader, 1).await;
    assert!(init_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

    let initialized_evt = read_next(&mut reader).await;
    assert!(is_event(&initialized_evt, "initialized"));

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
    let (attach_resp, _) = read_until_response(&mut reader, 2).await;
    assert!(attach_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

    send_request(&mut writer, 3, "terminate", json!({})).await;
    let (term_resp, term_messages) = read_until_response(&mut reader, 3).await;
    assert!(term_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

    // `terminated` can arrive before or after the terminate response depending on scheduling.
    let mut saw_terminated = term_messages.iter().any(|msg| is_event(msg, "terminated"));
    for _ in 0..50 {
        if saw_terminated {
            break;
        }
        let msg = read_next(&mut reader).await;
        saw_terminated = is_event(&msg, "terminated");
    }
    assert!(saw_terminated, "expected terminated event after terminate");

    server_task.await.unwrap().unwrap();

    let exit_codes = jdwp.virtual_machine_exit_codes().await;
    assert!(
        exit_codes.last().copied() == Some(0),
        "expected VirtualMachine.Exit to be called with 0, got {exit_codes:?}"
    );
}
