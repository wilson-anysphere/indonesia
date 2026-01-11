use serde_json::{json, Value};
use tempfile::TempDir;

use nova_dap::dap_tokio::{DapReader, DapWriter};
use nova_dap::wire_server;
use nova_jdwp::wire::mock::{MockJdwpServer, MockJdwpServerConfig};

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
async fn stack_trace_maps_sources_to_absolute_paths() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();

    // Make this a minimal Maven workspace so `nova_project::load_project` can infer
    // standard source roots such as `src/main/java`.
    std::fs::write(
        root.join("pom.xml"),
        r#"<project>
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>demo</artifactId>
  <version>1.0.0</version>
</project>
"#,
    )
    .unwrap();

    let src_path = root.join("src/main/java/com/example");
    std::fs::create_dir_all(&src_path).unwrap();
    let main_path = src_path.join("Main.java");
    std::fs::write(&main_path, "package com.example;\nclass Main {}\n").unwrap();

    let jdwp = MockJdwpServer::spawn_with_config(MockJdwpServerConfig {
        class_signature: "Lcom/example/Main;".to_string(),
        source_file: "Main.java".to_string(),
        ..Default::default()
    })
    .await
    .unwrap();

    let (client, server_stream) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_stream);
    let server_task =
        tokio::spawn(async move { wire_server::run(server_read, server_write).await });

    let (client_read, client_write) = tokio::io::split(client);
    let mut reader = DapReader::new(client_read);
    let mut writer = DapWriter::new(client_write);

    send_request(&mut writer, 1, "initialize", json!({})).await;
    let _init_resp = read_response(&mut reader, 1).await;

    send_request(
        &mut writer,
        2,
        "attach",
        json!({
            "host": "127.0.0.1",
            "port": jdwp.addr().port(),
            "projectRoot": root.to_string_lossy(),
        }),
    )
    .await;
    let attach_resp = read_response(&mut reader, 2).await;
    assert!(attach_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

    let _initialized = read_event(&mut reader, "initialized").await;

    send_request(&mut writer, 3, "threads", json!({})).await;
    let threads_resp = read_response(&mut reader, 3).await;
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
    let stack_resp = read_response(&mut reader, 4).await;

    let mapped = stack_resp
        .pointer("/body/stackFrames/0/source/path")
        .and_then(|v| v.as_str())
        .unwrap();
    let expected = std::fs::canonicalize(&main_path).unwrap();
    assert_eq!(mapped, expected.to_string_lossy().as_ref());

    send_request(&mut writer, 5, "disconnect", json!({})).await;
    let _disc_resp = read_response(&mut reader, 5).await;

    server_task.await.unwrap().unwrap();
}
