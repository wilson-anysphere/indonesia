use std::time::Duration;

use crate::harness::spawn_wire_server;
use nova_jdwp::wire::mock::MockJdwpServer;
use serde_json::json;

#[tokio::test]
async fn configuration_done_before_java_launch_does_not_deadlock_stop_on_entry() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();
    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;

    // Some DAP clients may send `configurationDone` before `launch`. Ensure the adapter
    // doesn't get stuck awaiting a second configurationDone once `launch` completes.
    let cfg_resp = client.request("configurationDone", json!({})).await;
    assert_eq!(
        cfg_resp.get("success").and_then(|v| v.as_bool()),
        Some(true),
        "expected configurationDone to succeed: {cfg_resp}"
    );

    // Use the test helper binary as a stand-in for `java`. It ignores the JVM arguments but
    // keeps the process alive so `nova-dap` can treat this as a normal Java launch.
    let helper = env!("CARGO_BIN_EXE_nova_dap_test_helper");
    let launch_resp = client
        .request(
            "launch",
            json!({
                "javaPath": helper,
                "mainClass": "Main",
                "classpath": ["."],
                "stopOnEntry": true,
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

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        if jdwp.vm_resume_calls() > 0 {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "expected VM to be resumed after launch, but vm_resume_calls={}",
                jdwp.vm_resume_calls()
            );
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}
