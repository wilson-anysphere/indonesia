use std::time::Duration;

use serde_json::json;
use tokio::io::split;

use nova_dap::dap_tokio::{DapReader, DapWriter};

#[cfg(debug_assertions)]
#[tokio::test]
async fn slow_client_does_not_deadlock_disconnect() {
    // Small duplex buffer to make the server's writer hit backpressure quickly when the client
    // doesn't read.
    let (server_stream, client_stream) = tokio::io::duplex(128);

    let (server_read, server_write) = split(server_stream);
    let (client_read, client_write) = split(client_stream);

    let server_task =
        tokio::spawn(async move { nova_dap::wire_server::run(server_read, server_write).await });

    let mut writer = DapWriter::new(client_write);

    writer
        .write_value(&json!({
            "seq": 1,
            "type": "request",
            "command": "initialize",
            "arguments": {},
        }))
        .await
        .expect("initialize write");

    // Read the initialize response to ensure the stream is in sync before we intentionally stop
    // reading (to simulate a slow client).
    let mut reader = DapReader::new(client_read);
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let Some(msg) = reader.read_value().await.expect("DAP read failed") else {
                panic!("server closed stream during initialize");
            };
            if msg.get("type").and_then(|v| v.as_str()) == Some("response")
                && msg.get("request_seq").and_then(|v| v.as_i64()) == Some(1)
            {
                break;
            }
        }
    })
    .await
    .expect("timed out waiting for initialize response");

    // Flood output without reading responses/events to simulate a very chatty debuggee and a slow
    // client. This should not grow memory unboundedly and should still allow `disconnect` to
    // complete once the client begins reading again.
    writer
        .write_value(&json!({
            "seq": 2,
            "type": "request",
            "command": "nova/testFloodOutput",
            "arguments": {
                "count": 50_000,
                "output": "hello world\\n",
            },
        }))
        .await
        .expect("flood write");

    // Give the server a moment to enqueue output and hit backpressure while we aren't reading.
    tokio::time::sleep(Duration::from_millis(100)).await;

    writer
        .write_value(&json!({
            "seq": 3,
            "type": "request",
            "command": "disconnect",
            "arguments": {},
        }))
        .await
        .expect("disconnect write");

    // Keep not reading for a bit to ensure the server's writer is blocked.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Start reading again and ensure we eventually see the disconnect response.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let Some(msg) = reader.read_value().await.expect("DAP read failed") else {
                panic!("server closed the stream before sending a disconnect response");
            };

            if msg.get("type").and_then(|v| v.as_str()) == Some("response")
                && msg.get("request_seq").and_then(|v| v.as_i64()) == Some(3)
            {
                break;
            }
        }
    })
    .await
    .expect("timed out waiting for disconnect response");

    let server_result = tokio::time::timeout(Duration::from_secs(3), server_task)
        .await
        .expect("server did not exit")
        .expect("server task panicked");

    assert!(
        server_result.is_ok(),
        "server returned error: {server_result:?}"
    );
}
