use serde_json::{json, Value};

use nova_dap::dap_tokio::{DapReader, DapWriter};
use nova_dap::wire_server;

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
    for _ in 0..200 {
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
    for _ in 0..200 {
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
#[ignore = "requires a local JDK (java/javac)"]
async fn dap_can_launch_a_jvm_and_forward_output() {
    let temp = tempfile::TempDir::new().unwrap();
    let root = temp.path();
    let source_path = root.join("Main.java");
    std::fs::write(
        &source_path,
        r#"public class Main {
  public static void main(String[] args) throws Exception {
    System.out.println("hello stdout");
    System.err.println("hello stderr");
  }
}
"#,
    )
    .unwrap();

    let javac = std::process::Command::new("javac")
        .arg(&source_path)
        .current_dir(root)
        .status();
    let Ok(javac_status) = javac else {
        // Allow manual runs in environments without a JDK without producing a hard failure.
        return;
    };
    assert!(javac_status.success());

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
        "launch",
        json!({
            "mainClass": "Main",
            "classpath": [root.to_string_lossy()],
            "cwd": root.to_string_lossy(),
            "stopOnEntry": true,
            "attachTimeoutMs": 10_000,
        }),
    )
    .await;
    let launch_resp = read_response(&mut reader, 2).await;
    assert!(launch_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

    let _initialized = read_event(&mut reader, "initialized").await;

    // Configuration done should resume the debuggee and allow the process to run.
    send_request(&mut writer, 3, "configurationDone", json!({})).await;
    let config_resp = read_response(&mut reader, 3).await;
    assert!(config_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

    let mut saw_stdout = false;
    let mut saw_stderr = false;
    let mut saw_terminated = false;

    for _ in 0..400 {
        let msg = read_next(&mut reader).await;
        if msg.get("type").and_then(|v| v.as_str()) != Some("event") {
            continue;
        }
        match msg.get("event").and_then(|v| v.as_str()) {
            Some("output") => match msg.pointer("/body/category").and_then(|v| v.as_str()) {
                Some("stdout") => saw_stdout = true,
                Some("stderr") => saw_stderr = true,
                _ => {}
            },
            Some("terminated") => {
                saw_terminated = true;
                break;
            }
            _ => {}
        }
    }

    assert!(saw_stdout, "expected at least one stdout output event");
    assert!(saw_stderr, "expected at least one stderr output event");
    assert!(saw_terminated, "expected terminated event");

    server_task.await.unwrap().unwrap();
}
