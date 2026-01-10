use nova_jdwp::wire::{mock::MockJdwpServer, JdwpClient, JdwpValue};

#[tokio::test]
async fn jdwp_client_can_handshake_and_fetch_values() {
    let server = MockJdwpServer::spawn().await.unwrap();
    let client = JdwpClient::connect(server.addr()).await.unwrap();

    let threads = client.all_threads().await.unwrap();
    assert_eq!(threads.len(), 1);

    let thread = threads[0];
    let name = client.thread_name(thread).await.unwrap();
    assert_eq!(name, "main");

    let frames = client.frames(thread, 0, 10).await.unwrap();
    assert_eq!(frames.len(), 1);

    let frame = frames[0];
    let (_argc, vars) = client
        .method_variable_table(frame.location.class_id, frame.location.method_id)
        .await
        .unwrap();
    assert_eq!(vars.len(), 2);

    let slots: Vec<(u32, String)> = vars.iter().map(|v| (v.slot, v.signature.clone())).collect();
    let values = client
        .stack_frame_get_values(thread, frame.frame_id, &slots)
        .await
        .unwrap();
    assert_eq!(values.len(), 2);
    assert_eq!(values[0], JdwpValue::Int(42));

    let object_id = match values[1] {
        JdwpValue::Object { id, .. } => id,
        _ => panic!("expected object value"),
    };

    let class_id = client.object_reference_reference_type(object_id).await.unwrap();
    let fields = client.reference_type_fields(class_id).await.unwrap();
    assert_eq!(fields.len(), 1);

    let field_ids: Vec<u64> = fields.iter().map(|f| f.field_id).collect();
    let field_values = client.object_reference_get_values(object_id, &field_ids).await.unwrap();
    assert_eq!(field_values.len(), 1);
    assert_eq!(field_values[0], JdwpValue::Int(7));
}
