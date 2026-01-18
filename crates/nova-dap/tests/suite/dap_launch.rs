use std::{path::Path, time::Duration};

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

#[tokio::test]
async fn dap_launch_spawns_process_forwards_output_and_disconnect_can_terminate() {
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

    // Ensure at least one stdout/stderr output event is forwarded and we observe the `process`
    // event emitted for the launch.
    let mut saw_output = messages.iter().any(|msg| {
        msg.get("type").and_then(|v| v.as_str()) == Some("event")
            && msg.get("event").and_then(|v| v.as_str()) == Some("output")
    });
    let mut process_evt = find_event(&messages, "process").cloned();
    for _ in 0..50 {
        if saw_output && process_evt.is_some() {
            break;
        }
        let msg = read_next(&mut reader).await;
        if msg.get("type").and_then(|v| v.as_str()) == Some("event")
            && msg.get("event").and_then(|v| v.as_str()) == Some("output")
        {
            saw_output = true;
        }
        if msg.get("type").and_then(|v| v.as_str()) == Some("event")
            && msg.get("event").and_then(|v| v.as_str()) == Some("process")
        {
            process_evt = Some(msg.clone());
        }
        messages.push(msg);
    }
    assert!(
        saw_output,
        "expected at least one output event from launched process"
    );
    let process_evt = process_evt.expect("expected a process event after launch");
    let process_name = process_evt
        .pointer("/body/name")
        .and_then(|v| v.as_str())
        .expect("process event missing body.name");
    assert!(
        !process_name.is_empty(),
        "process event missing body.name: {process_evt}"
    );
    let process_pid = process_evt
        .pointer("/body/systemProcessId")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32)
        .expect("process event missing body.systemProcessId");

    // Wait for the pid file to show up so we can assert termination semantics.
    let pid: u32 = {
        let mut pid = None;
        for _ in 0..50 {
            if let Ok(contents) = std::fs::read_to_string(&pid_path) {
                if let Ok(p) = contents.trim().parse::<u32>() {
                    pid = Some(p);
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        pid.expect("pid file should be written by helper process")
    };
    assert_eq!(
        process_pid, pid,
        "process event systemProcessId did not match helper pid file"
    );

    #[cfg(target_os = "linux")]
    assert!(
        Path::new(&format!("/proc/{pid}")).exists(),
        "helper process should be running"
    );

    send_request(&mut writer, 3, "threads", json!({})).await;
    let (threads_resp, _) = read_until_response(&mut reader, 3).await;
    let thread_id = threads_resp
        .pointer("/body/threads/0/id")
        .and_then(|v| v.as_i64())
        .unwrap();

    send_request(
        &mut writer,
        4,
        "stackTrace",
        json!({ "threadId": thread_id }),
    )
    .await;
    let (stack_resp, _) = read_until_response(&mut reader, 4).await;
    let frames = stack_resp
        .pointer("/body/stackFrames")
        .and_then(|v| v.as_array())
        .unwrap();
    assert!(!frames.is_empty());

    send_request(
        &mut writer,
        5,
        "disconnect",
        json!({ "terminateDebuggee": true }),
    )
    .await;
    let (disc_resp, disc_messages) = read_until_response(&mut reader, 5).await;
    assert!(disc_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

    // `terminated` can arrive before or after the disconnect response depending on scheduling.
    let mut saw_terminated = find_event(&disc_messages, "terminated").is_some();
    for _ in 0..50 {
        if saw_terminated {
            break;
        }
        let msg = read_next(&mut reader).await;
        saw_terminated = msg.get("type").and_then(|v| v.as_str()) == Some("event")
            && msg.get("event").and_then(|v| v.as_str()) == Some("terminated");
        if saw_terminated {
            break;
        }
    }
    assert!(saw_terminated, "expected terminated event after disconnect");

    #[cfg(target_os = "linux")]
    {
        let proc_path = format!("/proc/{pid}");
        let mut exited = false;
        for _ in 0..50 {
            if !Path::new(&proc_path).exists() {
                exited = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(exited, "expected helper process {pid} to be terminated");
    }

    server_task.await.unwrap().unwrap();
}
