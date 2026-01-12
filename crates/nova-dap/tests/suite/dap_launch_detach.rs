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

async fn read_next(
    reader: &mut DapReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
) -> Option<Value> {
    tokio::time::timeout(Duration::from_secs(5), reader.read_value())
        .await
        .expect("timed out waiting for DAP message")
        .unwrap()
}

async fn read_until_response(
    reader: &mut DapReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    request_seq: i64,
) -> (Value, Vec<Value>) {
    let mut other = Vec::new();
    for _ in 0..200 {
        let msg = read_next(reader)
            .await
            .expect("DAP stream closed unexpectedly");
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

fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
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
                .args(["/PID", &pid.to_string(), "/F"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

#[cfg(unix)]
fn pid_exists(pid: u32) -> bool {
    let res = unsafe { libc::kill(pid as i32, 0) };
    if res == 0 {
        return true;
    }

    match std::io::Error::last_os_error().raw_os_error() {
        Some(code) if code == libc::ESRCH => false,
        // Even if we're not allowed to signal the process, we know it exists.
        Some(code) if code == libc::EPERM => true,
        _ => false,
    }
}

#[tokio::test]
async fn dap_launch_disconnect_terminate_debuggee_false_detaches_without_exited_event() {
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

    let initialized_evt = read_next(&mut reader).await.unwrap();
    assert!(is_event(&initialized_evt, "initialized"));

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
                "--heartbeat",
                "--heartbeat-file",
                heartbeat_path.to_string_lossy(),
                "--sleep-ms",
                "50"
            ],
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

    let pid = wait_for_pid_file(&pid_path).await;
    let _kill = KillOnDrop(pid);
    let heartbeat_len_before_detach = file_len(&heartbeat_path);

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

    assert!(
        !disc_messages.iter().any(|msg| is_event(msg, "exited")),
        "did not expect exited event when disconnecting with terminateDebuggee=false"
    );

    let mut saw_terminated = disc_messages.iter().any(|msg| is_event(msg, "terminated"));
    for _ in 0..50 {
        if saw_terminated {
            break;
        }
        let Some(msg) = read_next(&mut reader).await else {
            break;
        };
        assert!(
            !is_event(&msg, "exited"),
            "did not expect exited event when disconnecting with terminateDebuggee=false"
        );
        if is_event(&msg, "terminated") {
            saw_terminated = true;
        }
    }
    assert!(saw_terminated, "expected terminated event after disconnect");

    // Wait for the adapter to fully shut down so the debuggee's stdout/stderr pipes are closed.
    server_task.await.unwrap().unwrap();

    // The helper should keep running after detach. Assert this by observing that it continues
    // appending to a file even after the adapter exits (stdout/stderr are pipes that have been
    // closed at this point).
    let mut heartbeat_grew = false;
    for _ in 0..50 {
        if file_len(&heartbeat_path) > heartbeat_len_before_detach {
            heartbeat_grew = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        heartbeat_grew,
        "expected helper to keep running and writing heartbeats after detach"
    );

    // The launched process should remain running after the adapter detaches.
    #[cfg(unix)]
    {
        let mut still_running = false;
        for _ in 0..50 {
            if pid_exists(pid) {
                still_running = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            still_running,
            "expected helper process {pid} to still be running after detach"
        );
    }
}
