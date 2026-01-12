use std::time::Duration;

use serde_json::{json, Value};

use nova_dap::dap_tokio::{DapReader, DapWriter};
use nova_dap::wire_server;
use nova_jdwp::wire::mock::MockJdwpServer;
use tempfile::TempDir;

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
    tokio::time::timeout(Duration::from_secs(5), reader.read_value())
        .await
        .expect("timed out waiting for DAP message")
        .unwrap()
        .expect("DAP stream closed unexpectedly")
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

fn find_event<'a>(messages: &'a [Value], name: &str) -> Option<&'a Value> {
    messages.iter().find(|msg| {
        msg.get("type").and_then(|v| v.as_str()) == Some("event")
            && msg.get("event").and_then(|v| v.as_str()) == Some(name)
    })
}

#[tokio::test]
async fn dap_launch_emits_exited_event_with_exit_code() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();

    let (client, server_stream) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_stream);
    let server_task =
        tokio::spawn(async move { wire_server::run(server_read, server_write).await });

    let (client_read, client_write) = tokio::io::split(client);
    let mut reader = DapReader::new(client_read);
    let mut writer = DapWriter::new(client_write);

    send_request(&mut writer, 1, "initialize", json!({})).await;
    let (init_resp, _) = read_until_response(&mut reader, 1).await;
    assert!(init_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

    // Per DAP spec, the adapter emits `initialized` after responding to `initialize`.
    let initialized_evt = read_next(&mut reader).await;
    assert_eq!(
        initialized_evt.get("event").and_then(|v| v.as_str()),
        Some("initialized")
    );

    let temp = TempDir::new().unwrap();
    let helper = env!("CARGO_BIN_EXE_nova_dap_test_helper");

    send_request(
        &mut writer,
        2,
        "launch",
        json!({
            "cwd": temp.path().to_string_lossy(),
            "command": helper,
            "args": ["--exit-after-ms", "0", "--exit-code", "42"],
            "env": { "NOVA_DAP_TEST": "1" },
            "host": "127.0.0.1",
            "port": jdwp.addr().port(),
            "attachTimeoutMs": 2_000,
        }),
    )
    .await;

    let (launch_resp, mut messages) = read_until_response(&mut reader, 2).await;
    assert!(launch_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

    let exited_evt = loop {
        if let Some(evt) = find_event(&messages, "exited") {
            break evt.clone();
        }
        messages.push(read_next(&mut reader).await);
        if messages.len() > 200 {
            panic!("did not receive exited event");
        }
    };

    assert_eq!(
        exited_evt
            .pointer("/body/exitCode")
            .and_then(|v| v.as_i64()),
        Some(42)
    );

    // `terminated` should follow `exited`, but accept either ordering to avoid scheduling races.
    let mut saw_terminated = find_event(&messages, "terminated").is_some();
    for _ in 0..200 {
        if saw_terminated {
            break;
        }
        let msg = read_next(&mut reader).await;
        saw_terminated = msg.get("type").and_then(|v| v.as_str()) == Some("event")
            && msg.get("event").and_then(|v| v.as_str()) == Some("terminated");
        messages.push(msg);
    }
    assert!(saw_terminated, "expected terminated event after exited");

    server_task.await.unwrap().unwrap();
}
