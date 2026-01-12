use std::{path::Path, time::Duration};

use nova_dap::dap_tokio::{DapReader, DapWriter};
use nova_dap::wire_server;
use nova_jdwp::wire::mock::MockJdwpServer;
use serde_json::{json, Value};
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

async fn wait_for_pid_file(path: &Path) -> u32 {
    for _ in 0..100 {
        if let Ok(contents) = std::fs::read_to_string(path) {
            if let Ok(pid) = contents.trim().parse::<u32>() {
                return pid;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("pid file {path:?} was not written");
}

async fn wait_for_new_pid(path: &Path, old_pid: u32) -> u32 {
    for _ in 0..200 {
        if let Ok(contents) = std::fs::read_to_string(path) {
            if let Ok(pid) = contents.trim().parse::<u32>() {
                if pid != old_pid {
                    return pid;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("pid file {path:?} was not updated after restart");
}

#[tokio::test]
async fn dap_restart_command_launch_terminates_old_process_spawns_new_and_adapter_stays_alive() {
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
    let (launch_resp, _) = read_until_response(&mut reader, 2).await;
    assert!(launch_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

    let pid1 = wait_for_pid_file(&pid_path).await;

    #[cfg(target_os = "linux")]
    assert!(
        Path::new(&format!("/proc/{pid1}")).exists(),
        "expected helper process to be running before restart"
    );

    send_request(&mut writer, 3, "restart", json!({})).await;
    let (restart_resp, _) = read_until_response(&mut reader, 3).await;
    assert!(restart_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

    let pid2 = wait_for_new_pid(&pid_path, pid1).await;
    assert_ne!(pid1, pid2, "expected restart to spawn a new process");

    #[cfg(target_os = "linux")]
    {
        let proc1 = format!("/proc/{pid1}");
        let proc2 = format!("/proc/{pid2}");

        let mut old_exited = false;
        for _ in 0..200 {
            if !Path::new(&proc1).exists() {
                old_exited = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(old_exited, "expected old helper process {pid1} to exit");
        assert!(
            Path::new(&proc2).exists(),
            "expected new helper process {pid2} to be running"
        );
    }

    // Ensure the adapter remains responsive after restart.
    send_request(&mut writer, 4, "threads", json!({})).await;
    let (threads_resp, _) = read_until_response(&mut reader, 4).await;
    assert!(threads_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

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
    let mut saw_terminated = disc_messages.iter().any(|msg| {
        msg.get("type").and_then(|v| v.as_str()) == Some("event")
            && msg.get("event").and_then(|v| v.as_str()) == Some("terminated")
    });
    for _ in 0..50 {
        if saw_terminated {
            break;
        }
        let msg = read_next(&mut reader).await;
        saw_terminated = msg.get("type").and_then(|v| v.as_str()) == Some("event")
            && msg.get("event").and_then(|v| v.as_str()) == Some("terminated");
    }
    assert!(saw_terminated, "expected terminated event after disconnect");

    server_task.await.unwrap().unwrap();
}
