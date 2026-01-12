use std::{path::Path, time::Duration};

#[cfg(target_os = "windows")]
use std::process::{Command as StdCommand, Stdio};

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

fn process_event_pid(msg: &Value) -> Option<u32> {
    if msg.get("type").and_then(|v| v.as_str()) != Some("event") {
        return None;
    }
    if msg.get("event").and_then(|v| v.as_str()) != Some("process") {
        return None;
    }
    msg.get("body")
        .and_then(|b| b.get("systemProcessId"))
        .and_then(|v| v.as_u64())
        .and_then(|pid| u32::try_from(pid).ok())
}

async fn read_until_process_event(
    reader: &mut DapReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    buffered: &[Value],
) -> Value {
    for msg in buffered {
        if process_event_pid(msg).is_some() {
            return msg.clone();
        }
    }

    for _ in 0..200 {
        let msg = tokio::time::timeout(Duration::from_secs(2), read_next(reader))
            .await
            .expect("timed out waiting for process event");
        if process_event_pid(&msg).is_some() {
            return msg;
        }
    }

    panic!("did not receive process event with systemProcessId");
}

fn is_event(msg: &Value, name: &str) -> bool {
    msg.get("type").and_then(|v| v.as_str()) == Some("event")
        && msg.get("event").and_then(|v| v.as_str()) == Some(name)
}

fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

async fn wait_for_file_contains(path: &Path, needle: &str) {
    for _ in 0..200 {
        if let Ok(contents) = std::fs::read_to_string(path) {
            if contents.contains(needle) {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("file {path:?} did not contain {needle:?}");
}

struct KillOnDrop(u32);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let pid = self.0;
        if pid == 0 {
            return;
        }

        #[cfg(unix)]
        unsafe {
            let _ = libc::kill(pid as i32, libc::SIGKILL);
        }

        #[cfg(target_os = "windows")]
        {
            let _ = StdCommand::new("taskkill")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
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
    let heartbeat_path = temp.path().join("heartbeat.txt");

    let helper = env!("CARGO_BIN_EXE_nova_dap_test_helper");
    send_request(
        &mut writer,
        2,
        "launch",
        json!({
            "cwd": temp.path().to_string_lossy(),
            "command": helper,
            "args": [
                "--pid-file",
                pid_path.to_string_lossy(),
                "--heartbeat-file",
                heartbeat_path.to_string_lossy(),
                "--sleep-ms",
                "25"
            ],
            "env": { "NOVA_DAP_TEST": "1" },
            "host": "127.0.0.1",
            "port": jdwp.addr().port(),
            "attachTimeoutMs": 2_000,
        }),
    )
    .await;
    let (launch_resp, launch_messages) = read_until_response(&mut reader, 2).await;
    assert!(launch_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

    let pid1 = wait_for_pid_file(&pid_path).await;
    let launch_process_evt = read_until_process_event(&mut reader, &launch_messages).await;
    assert_eq!(
        launch_process_evt
            .pointer("/body/startMethod")
            .and_then(|v| v.as_str()),
        Some("launch"),
        "expected process event startMethod=launch after launch: {launch_process_evt}"
    );
    let process_pid1 = process_event_pid(&launch_process_evt)
        .expect("expected process event to include body.systemProcessId after launch");
    assert_eq!(
        process_pid1, pid1,
        "expected process event pid to match pid file after launch"
    );
    wait_for_file_contains(&heartbeat_path, &format!("heartbeat pid={pid1}")).await;

    #[cfg(target_os = "linux")]
    assert!(
        Path::new(&format!("/proc/{pid1}")).exists(),
        "expected helper process to be running before restart"
    );

    send_request(&mut writer, 3, "restart", json!({})).await;
    let (restart_resp, restart_messages) = read_until_response(&mut reader, 3).await;
    assert!(restart_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));
    assert!(
        !restart_messages
            .iter()
            .any(|msg| is_event(msg, "terminated") || is_event(msg, "exited")),
        "did not expect terminated/exited events during restart"
    );

    let pid2 = wait_for_new_pid(&pid_path, pid1).await;
    assert_ne!(pid1, pid2, "expected restart to spawn a new process");
    let restart_process_evt = read_until_process_event(&mut reader, &restart_messages).await;
    let process_pid2 = process_event_pid(&restart_process_evt)
        .expect("expected process event to include body.systemProcessId after restart");
    assert_eq!(
        process_pid2, pid2,
        "expected process event pid to match pid file after restart"
    );
    assert_ne!(
        process_pid1, process_pid2,
        "expected restart to emit a process event for the new process"
    );
    let _kill = KillOnDrop(pid2);
    wait_for_file_contains(&heartbeat_path, &format!("heartbeat pid={pid2}")).await;

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

    // Ensure the old process is not still producing heartbeats after the new process starts.
    // (This is a coarse cross-platform check to catch regressions where `restart` doesn't kill the
    // old debuggee.)
    let len_at_pid2 = file_len(&heartbeat_path) as usize;
    tokio::time::sleep(Duration::from_millis(200)).await;
    if let Ok(bytes) = std::fs::read(&heartbeat_path) {
        let start = len_at_pid2.min(bytes.len());
        let new_text = String::from_utf8_lossy(&bytes[start..]);
        assert!(
            !new_text.contains(&format!("heartbeat pid={pid1}")),
            "expected old process {pid1} to stop writing heartbeats after restart; saw: {new_text:?}"
        );
        assert!(
            new_text.contains(&format!("heartbeat pid={pid2}")),
            "expected new process {pid2} to be writing heartbeats after restart; saw: {new_text:?}"
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
