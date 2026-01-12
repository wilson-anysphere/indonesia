use crate::harness::spawn_wire_server;
use nova_jdwp::wire::mock::MockJdwpServer;

#[tokio::test]
async fn attach_accepts_hostname_localhost() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();
    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;

    // `localhost` may resolve to `::1` before `127.0.0.1` depending on the environment.
    // The mock JDWP server binds only to IPv4, so the adapter must prefer IPv4 and/or
    // fall back to the IPv4 address when attaching.
    client.attach("localhost", jdwp.addr().port()).await;

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}
