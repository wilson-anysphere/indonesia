use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use serde_json::{json, Value};

use nova_dap::dap_tokio::{DapReader, DapWriter};
use nova_dap::wire_server;
use nova_jdwp::wire::mock::{DelayedReply, MockJdwpServer, MockJdwpServerConfig};

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

async fn read_responses(
    reader: &mut DapReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    request_seqs: &[i64],
) -> HashMap<i64, Value> {
    let mut remaining: HashSet<i64> = request_seqs.iter().copied().collect();
    let mut out = HashMap::new();

    for _ in 0..100 {
        let msg = read_next(reader).await;
        if msg.get("type").and_then(|v| v.as_str()) != Some("response") {
            continue;
        }
        let Some(req_seq) = msg.get("request_seq").and_then(|v| v.as_i64()) else {
            continue;
        };
        if remaining.remove(&req_seq) {
            out.insert(req_seq, msg);
            if remaining.is_empty() {
                return out;
            }
        }
    }

    panic!("did not receive responses for request seqs {request_seqs:?}");
}

#[tokio::test]
async fn dap_cancel_aborts_long_running_request() {
    let jdwp = MockJdwpServer::spawn_with_config(MockJdwpServerConfig {
        delayed_replies: vec![DelayedReply {
            command_set: 11,
            command: 6, // ThreadReference.Frames
            delay: Duration::from_secs(5),
        }],
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
    let init_resp = read_response(&mut reader, 1).await;
    assert!(init_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

    send_request(
        &mut writer,
        2,
        "attach",
        json!({
            "host": "127.0.0.1",
            "port": jdwp.addr().port()
        }),
    )
    .await;
    let attach_resp = read_response(&mut reader, 2).await;
    assert!(attach_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

    let initialized = read_event(&mut reader, "initialized").await;
    assert_eq!(
        initialized.get("event").and_then(|v| v.as_str()),
        Some("initialized")
    );

    send_request(&mut writer, 3, "threads", json!({})).await;
    let threads_resp = read_response(&mut reader, 3).await;
    let thread_id = threads_resp
        .pointer("/body/threads/0/id")
        .and_then(|v| v.as_i64())
        .unwrap();

    // This stackTrace request will block on the delayed JDWP reply.
    send_request(
        &mut writer,
        4,
        "stackTrace",
        json!({ "threadId": thread_id }),
    )
    .await;
    send_request(&mut writer, 5, "cancel", json!({ "requestId": 4 })).await;

    let responses = read_responses(&mut reader, &[4, 5]).await;

    let cancel_resp = responses.get(&5).unwrap();
    assert!(cancel_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

    let stack_resp = responses.get(&4).unwrap();
    assert!(!stack_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(true));
    assert_eq!(
        stack_resp.get("message").and_then(|v| v.as_str()),
        Some("cancelled")
    );

    // Ensure the server remains responsive after cancellation (JDWP may still have a delayed reply pending).
    send_request(&mut writer, 6, "threads", json!({})).await;
    let threads_resp = read_response(&mut reader, 6).await;
    assert!(threads_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

    send_request(&mut writer, 7, "disconnect", json!({})).await;
    let _disc_resp = read_response(&mut reader, 7).await;

    server_task.await.unwrap().unwrap();
}
