use nova_dap::jdwp::wire::{mock::MockJdwpServer, JdwpClient, JdwpError, JdwpValue};

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

    let class_id = client
        .object_reference_reference_type(object_id)
        .await
        .unwrap();
    let fields = client.reference_type_fields(class_id).await.unwrap();
    assert_eq!(fields.len(), 1);

    let field_ids: Vec<u64> = fields.iter().map(|f| f.field_id).collect();
    let field_values = client
        .object_reference_get_values(object_id, &field_ids)
        .await
        .unwrap();
    assert_eq!(field_values.len(), 1);
    assert_eq!(field_values[0], JdwpValue::Int(7));
}

#[tokio::test]
async fn jdwp_client_classes_by_signature() {
    let server = MockJdwpServer::spawn().await.unwrap();
    let client = JdwpClient::connect(server.addr()).await.unwrap();

    let all = client.all_classes().await.unwrap();
    assert_eq!(all.len(), 1);

    let by_sig = client.classes_by_signature("LMain;").await.unwrap();
    assert_eq!(by_sig.len(), 1);
    assert_eq!(by_sig[0].signature, "LMain;");
    assert_eq!(by_sig[0].type_id, all[0].type_id);
    assert_eq!(by_sig[0].ref_type_tag, all[0].ref_type_tag);
    assert_eq!(by_sig[0].status, all[0].status);
}

#[tokio::test]
async fn jdwp_client_redefine_class_by_name_succeeds_and_records_payload() {
    let server = MockJdwpServer::spawn().await.unwrap();
    let client = JdwpClient::connect(server.addr()).await.unwrap();

    let bytecode = vec![0xCA, 0xFE, 0xBA, 0xBE];
    client
        .redefine_class_by_name("com.example.Foo", &bytecode)
        .await
        .unwrap();

    // The mock server only knows about `Lcom/example/Foo;`, so a successful redefine
    // implies the client performed the correct name -> signature conversion.
    let infos = client
        .classes_by_signature("Lcom/example/Foo;")
        .await
        .unwrap();
    assert_eq!(infos.len(), 1);

    let calls = server.redefine_classes_calls().await;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].class_count, 1);
    assert_eq!(calls[0].classes.len(), 1);
    assert_eq!(calls[0].classes[0].0, infos[0].type_id);
    assert_eq!(calls[0].classes[0].1, bytecode);
}

#[tokio::test]
async fn jdwp_client_redefine_classes_propagates_vm_error_code() {
    let server = MockJdwpServer::spawn().await.unwrap();
    server.set_redefine_classes_error_code(67);
    let client = JdwpClient::connect(server.addr()).await.unwrap();

    let classes = client.all_classes().await.unwrap();
    let class_id = classes[0].type_id;
    let err = client
        .redefine_classes(&[(class_id, vec![1, 2, 3])])
        .await
        .unwrap_err();

    match err {
        JdwpError::VmError(code) => assert_eq!(code, 67),
        other => panic!("expected VmError, got {other:?}"),
    }
}

#[tokio::test]
async fn jdwp_client_string_reference_value() {
    let server = MockJdwpServer::spawn().await.unwrap();
    let client = JdwpClient::connect(server.addr()).await.unwrap();

    let value = client.string_reference_value(0xABCD).await.unwrap();
    assert_eq!(value, "mock string");
}

#[tokio::test]
async fn jdwp_client_object_reference_collection_controls() {
    let server = MockJdwpServer::spawn().await.unwrap();
    let client = JdwpClient::connect(server.addr()).await.unwrap();

    let object_id = 0xDEAD_BEEF;
    client
        .object_reference_disable_collection(object_id)
        .await
        .unwrap();
    assert!(server.pinned_object_ids().await.contains(&object_id));

    client
        .object_reference_enable_collection(object_id)
        .await
        .unwrap();
    assert!(!server.pinned_object_ids().await.contains(&object_id));
}
