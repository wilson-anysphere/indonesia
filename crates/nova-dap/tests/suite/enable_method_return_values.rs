use crate::harness::spawn_wire_server;
use nova_jdwp::wire::mock::MockJdwpServer;
use serde_json::json;

#[tokio::test]
async fn dap_enable_method_return_values_succeeds_when_supported() {
    let mut caps = vec![false; 32];
    caps[22] = true;
    let jdwp = MockJdwpServer::spawn_with_capabilities(caps).await.unwrap();

    let (client, server_task) = spawn_wire_server();
    client.initialize_handshake().await;
    client.attach("127.0.0.1", jdwp.addr().port()).await;

    let resp = client
        .request("nova/enableMethodReturnValues", json!({}))
        .await;
    assert_eq!(resp.get("success").and_then(|v| v.as_bool()), Some(true));

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}
