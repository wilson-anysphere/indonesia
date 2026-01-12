mod harness;

use std::time::Duration;

use harness::spawn_wire_server;
use nova_jdwp::wire::mock::MockJdwpServer;
use serde_json::json;
use tempfile::TempDir;

#[tokio::test]
async fn configuration_done_resumes_command_launch_when_stop_on_entry_is_defaulted() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();
    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;

    let temp = TempDir::new().unwrap();
    let helper = env!("CARGO_BIN_EXE_nova_dap_test_helper");

    // Command-based launch sessions (e.g. Maven Surefire debug, Gradle --debug-jvm)
    // typically start the JVM suspended. Ensure `configurationDone` triggers a resume
    // when stopOnEntry is enabled (or defaulted).
    let launch_resp = client
        .request(
            "launch",
            json!({
                "cwd": temp.path().to_string_lossy(),
                "command": helper,
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

    let resume_before = jdwp.vm_resume_calls();

    let cfg_resp = client.request("configurationDone", json!({})).await;
    assert_eq!(
        cfg_resp.get("success").and_then(|v| v.as_bool()),
        Some(true),
        "expected configurationDone to succeed: {cfg_resp}"
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        if jdwp.vm_resume_calls() > resume_before {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "expected VM to be resumed after configurationDone, but vm_resume_calls={}",
                jdwp.vm_resume_calls()
            );
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

