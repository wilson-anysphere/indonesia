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
    reader.read_value().await.unwrap().unwrap()
}

async fn read_until_response(
    reader: &mut DapReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    request_seq: i64,
    max_messages: usize,
) -> Value {
    for _ in 0..max_messages {
        let msg = read_next(reader).await;
        if msg.get("type").and_then(|v| v.as_str()) == Some("response")
            && msg.get("request_seq").and_then(|v| v.as_i64()) == Some(request_seq)
        {
            return msg;
        }
    }
    panic!("did not receive response for seq {request_seq} after reading {max_messages} messages");
}

async fn read_until_event(
    reader: &mut DapReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    event: &str,
    max_messages: usize,
) -> Value {
    for _ in 0..max_messages {
        let msg = read_next(reader).await;
        if msg.get("type").and_then(|v| v.as_str()) == Some("event")
            && msg.get("event").and_then(|v| v.as_str()) == Some(event)
        {
            return msg;
        }
    }
    panic!("did not receive event {event} after reading {max_messages} messages");
}

fn assert_success(resp: &Value, context: &str) {
    let ok = resp.get("success").and_then(|v| v.as_bool()).unwrap_or(false);
    assert!(ok, "{context} response was not successful: {resp}");
}

#[tokio::test]
async fn outgoing_queue_backpressure_does_not_deadlock_under_output_spam() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();

    // Use a small duplex buffer to simulate a slow-ish client and force the server-side writer to
    // apply backpressure.
    let (client, server_stream) = tokio::io::duplex(8 * 1024);
    let (server_read, server_write) = tokio::io::split(server_stream);
    let server_task = tokio::spawn(async move { wire_server::run(server_read, server_write).await });

    let (client_read, client_write) = tokio::io::split(client);
    let mut reader = DapReader::new(client_read);
    let mut writer = DapWriter::new(client_write);

    send_request(&mut writer, 1, "initialize", json!({})).await;
    let init = read_until_response(&mut reader, 1, 32).await;
    assert_success(&init, "initialize");
    let _ = read_until_event(&mut reader, "initialized", 32).await;

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
            "args": [
                "--pid-file",
                pid_path.to_string_lossy(),
                "--spam-stdout-lines",
                "20000",
            ],
            "env": { "NOVA_DAP_TEST": "1" },
            "host": "127.0.0.1",
            "port": jdwp.addr().port(),
            "attachTimeoutMs": 2_000,
        }),
    )
    .await;

    // Wait for the pid file so we know the helper started (without reading any adapter output).
    for _ in 0..100 {
        if pid_path.is_file() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(pid_path.is_file(), "pid file was not written by helper");

    // Let the helper spam output while the client isn't reading to ensure the adapter's outgoing
    // buffering is exercised.
    tokio::time::sleep(Duration::from_millis(200)).await;

    send_request(&mut writer, 3, "threads", json!({})).await;

    let threads_resp = tokio::time::timeout(
        Duration::from_secs(10),
        read_until_response(&mut reader, 3, 10_000),
    )
    .await
    .expect("timed out waiting for threads response");
    assert_success(&threads_resp, "threads");

    // Send disconnect while output is still being produced. We don't assert that the response
    // makes it through under extreme backpressure; the important property is that the adapter can
    // shut down without deadlocking.
    send_request(
        &mut writer,
        4,
        "disconnect",
        json!({ "terminateDebuggee": true }),
    )
    .await;

    drop(reader);
    drop(writer);

    tokio::time::timeout(Duration::from_secs(10), server_task)
        .await
        .expect("timed out waiting for server task")
        .unwrap()
        .unwrap();
}

