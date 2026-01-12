use std::time::Duration;

use crate::harness::spawn_wire_server;
use nova_jdwp::wire::mock::MockJdwpServer;
use serde_json::json;
use tempfile::TempDir;

#[tokio::test]
async fn launch_truncates_very_long_stdout_lines() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();
    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;

    let temp = TempDir::new().unwrap();
    let helper = env!("CARGO_BIN_EXE_nova_dap_test_helper");
    let line_len: usize = 200_000;

    let launch_resp = client
        .request(
            "launch",
            json!({
                "cwd": temp.path().to_string_lossy(),
                "command": helper,
                "args": ["--print-line-len", line_len.to_string()],
                "env": { "NOVA_DAP_TEST": "1" },
                "host": "127.0.0.1",
                "port": jdwp.addr().port(),
                "attachTimeoutMs": 2_000,
            }),
        )
        .await;
    assert_eq!(
        launch_resp.get("success").and_then(|v| v.as_bool()),
        Some(true),
        "expected launch to succeed: {launch_resp}"
    );

    let output_evt = client
        .wait_for_event_matching("output(truncated stdout)", Duration::from_secs(5), |msg| {
            msg.get("type").and_then(|v| v.as_str()) == Some("event")
                && msg.get("event").and_then(|v| v.as_str()) == Some("output")
                && msg.pointer("/body/category").and_then(|v| v.as_str()) == Some("stdout")
                && msg
                    .pointer("/body/output")
                    .and_then(|v| v.as_str())
                    .map(|s| s.contains("<output truncated>"))
                    .unwrap_or(false)
        })
        .await;

    let output = output_evt
        .pointer("/body/output")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(
        output.len() < line_len,
        "expected output to be truncated ({} bytes), but got {} bytes",
        line_len,
        output.len()
    );

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}
