// Part of the consolidated `nova-dap` integration test harness (`tests/tests.rs`).
use std::{path::Path, time::Duration};

use nova_dap::dap_tokio::{DapReader, DapWriter};
use nova_dap::wire_server;
use nova_jdwp::wire::mock::MockJdwpServer;
use serde_json::{json, Value};
use tempfile::TempDir;

use crate::harness::spawn_wire_server;

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

fn find_event<'a>(messages: &'a [Value], name: &str) -> Option<&'a Value> {
    messages.iter().find(|msg| {
        msg.get("type").and_then(|v| v.as_str()) == Some("event")
            && msg.get("event").and_then(|v| v.as_str()) == Some(name)
    })
}

async fn read_until_event(
    reader: &mut DapReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    messages: &mut Vec<Value>,
    event: &str,
    max_iters: usize,
) -> Value {
    if let Some(found) = find_event(messages, event) {
        return found.clone();
    }

    for _ in 0..max_iters {
        let msg = read_next(reader).await;
        if msg.get("type").and_then(|v| v.as_str()) == Some("event")
            && msg.get("event").and_then(|v| v.as_str()) == Some(event)
        {
            return msg;
        }
        messages.push(msg);
    }

    panic!("did not receive {event} event");
}

#[tokio::test]
async fn dap_launch_emits_process_event() {
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

    let initialized_evt = read_next(&mut reader).await;
    assert_eq!(
        initialized_evt.get("event").and_then(|v| v.as_str()),
        Some("initialized")
    );

    let temp = TempDir::new().unwrap();
    let pid_path = temp.path().join("pid.txt");

    let helper = env!("CARGO_BIN_EXE_nova_dap_test_helper");
    send_request(
        &mut writer,
        2,
        "launch",
        json!({
            "cwd": temp.path().to_string_lossy(),
            "command": helper,
            "args": ["--pid-file", pid_path.to_string_lossy()],
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

    let process_evt = read_until_event(&mut reader, &mut messages, "process", 100).await;
    let body = process_evt
        .get("body")
        .and_then(|v| v.as_object())
        .expect("process event missing body");

    assert_eq!(
        body.get("startMethod").and_then(|v| v.as_str()),
        Some("launch")
    );
    assert_eq!(
        body.get("isLocalProcess").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(body.get("name").and_then(|v| v.as_str()), Some(helper));

    let event_pid = body
        .get("systemProcessId")
        .and_then(|v| v.as_u64())
        .expect("process event missing systemProcessId");
    assert!(event_pid > 0, "expected non-zero systemProcessId");

    let helper_pid: u64 = {
        let mut pid = None;
        for _ in 0..50 {
            if let Ok(contents) = std::fs::read_to_string(&pid_path) {
                if let Ok(p) = contents.trim().parse::<u64>() {
                    pid = Some(p);
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        pid.expect("pid file should be written by helper process")
    };
    assert_eq!(event_pid, helper_pid);

    // Ensure helper process is running before asking the adapter to terminate it.
    #[cfg(target_os = "linux")]
    assert!(
        Path::new(&format!("/proc/{helper_pid}")).exists(),
        "helper process should be running"
    );

    send_request(
        &mut writer,
        3,
        "disconnect",
        json!({ "terminateDebuggee": true }),
    )
    .await;
    let (disc_resp, mut disc_messages) = read_until_response(&mut reader, 3).await;
    assert!(disc_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

    // Drain until `terminated` arrives so server shutdown doesn't block on a full duplex buffer.
    let mut saw_terminated = find_event(&disc_messages, "terminated").is_some();
    for _ in 0..50 {
        if saw_terminated {
            break;
        }
        let msg = read_next(&mut reader).await;
        saw_terminated = msg.get("type").and_then(|v| v.as_str()) == Some("event")
            && msg.get("event").and_then(|v| v.as_str()) == Some("terminated");
        disc_messages.push(msg);
    }
    assert!(saw_terminated, "expected terminated event after disconnect");

    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_attach_emits_process_event() {
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

    let initialized_evt = read_next(&mut reader).await;
    assert_eq!(
        initialized_evt.get("event").and_then(|v| v.as_str()),
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

    let (attach_resp, mut messages) = read_until_response(&mut reader, 2).await;
    assert!(attach_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

    let process_evt = read_until_event(&mut reader, &mut messages, "process", 100).await;
    let body = process_evt
        .get("body")
        .and_then(|v| v.as_object())
        .expect("process event missing body");
    let name = body
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(
        !name.is_empty(),
        "process event missing body.name: {process_evt}"
    );
    assert_eq!(
        body.get("isLocalProcess").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        body.get("startMethod").and_then(|v| v.as_str()),
        Some("attach")
    );
    assert!(
        body.get("systemProcessId").is_none(),
        "attach process event should not include systemProcessId"
    );

    send_request(&mut writer, 3, "disconnect", json!({})).await;
    let (disc_resp, mut disc_messages) = read_until_response(&mut reader, 3).await;
    assert!(disc_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

    // Drain until `terminated` arrives so server shutdown doesn't block on a full duplex buffer.
    let mut saw_terminated = find_event(&disc_messages, "terminated").is_some();
    for _ in 0..50 {
        if saw_terminated {
            break;
        }
        let msg = read_next(&mut reader).await;
        saw_terminated = msg.get("type").and_then(|v| v.as_str()) == Some("event")
            && msg.get("event").and_then(|v| v.as_str()) == Some("terminated");
        disc_messages.push(msg);
    }
    assert!(saw_terminated, "expected terminated event after disconnect");

    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn attach_emits_process_event_with_name() {
    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;
    let _jdwp = client.attach_mock_jdwp().await;

    // `attach_mock_jdwp()` uses the harness `attach()` helper, which already waits for and
    // records the `process` event in the transcript.
    let transcript = client.take_transcript().await;
    let process_evt = transcript
        .iter()
        .find(|entry| {
            entry.message.get("type").and_then(|v| v.as_str()) == Some("event")
                && entry.message.get("event").and_then(|v| v.as_str()) == Some("process")
        })
        .map(|entry| entry.message.clone())
        .expect("expected a process event after attach");
    let name = process_evt
        .pointer("/body/name")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(
        !name.is_empty(),
        "process event missing body.name: {process_evt}"
    );

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}
