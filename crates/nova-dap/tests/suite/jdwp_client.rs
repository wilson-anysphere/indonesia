use nova_dap::jdwp::wire::{
    mock::MockJdwpServer, EventModifier, JdwpClient, JdwpError, JdwpEvent, JdwpValue,
};

#[tokio::test]
async fn jdwp_client_can_handshake_and_fetch_values() {
    let server = MockJdwpServer::spawn().await.unwrap();
    let client = JdwpClient::connect(server.addr()).await.unwrap();

    let caps = client.capabilities().await;
    assert!(!caps.supports_redefine_classes());

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
    assert_eq!(vars.len(), 4);

    let slots: Vec<(u32, String)> = vars.iter().map(|v| (v.slot, v.signature.clone())).collect();
    let values = client
        .stack_frame_get_values(thread, frame.frame_id, &slots)
        .await
        .unwrap();
    assert_eq!(values.len(), 4);
    assert_eq!(values[0], JdwpValue::Int(42));

    let object_id = match values[1] {
        JdwpValue::Object { id, .. } => id,
        _ => panic!("expected object value"),
    };

    let (ref_type_tag, class_id) = client
        .object_reference_reference_type(object_id)
        .await
        .unwrap();
    assert_eq!(ref_type_tag, 1);
    let fields = client.reference_type_fields(class_id).await.unwrap();
    assert_eq!(fields.len(), 1);

    let field_ids: Vec<u64> = fields.iter().map(|f| f.field_id).collect();
    let field_values = client
        .object_reference_get_values(object_id, &field_ids)
        .await
        .unwrap();
    assert_eq!(field_values.len(), 1);
    assert_eq!(field_values[0], JdwpValue::Int(7));

    // String reference value.
    let string_id = match values[2] {
        JdwpValue::Object { id, .. } => id,
        _ => panic!("expected string object value"),
    };
    let string_value = client.string_reference_value(string_id).await.unwrap();
    assert_eq!(string_value, "mock string");

    // Array length + values.
    let array_id = match values[3] {
        JdwpValue::Object { id, .. } => id,
        _ => panic!("expected array object value"),
    };
    let len = client.array_reference_length(array_id).await.unwrap();
    assert_eq!(len, 3);
    let values = client
        .array_reference_get_values(array_id, 0, 3)
        .await
        .unwrap();
    assert_eq!(
        values,
        vec![JdwpValue::Int(0), JdwpValue::Int(1), JdwpValue::Int(2)]
    );

    // Object pinning primitives.
    client
        .object_reference_disable_collection(object_id)
        .await
        .unwrap();
    client
        .object_reference_enable_collection(object_id)
        .await
        .unwrap();
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

    let value = client
        .string_reference_value(server.string_object_id())
        .await
        .unwrap();
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

#[tokio::test]
async fn jdwp_client_can_fetch_array_values() {
    let server = MockJdwpServer::spawn().await.unwrap();
    let client = JdwpClient::connect(server.addr()).await.unwrap();

    let array_id = server.sample_int_array_id();
    let len = client.array_reference_length(array_id).await.unwrap();
    assert_eq!(len, 5);

    let values = client
        .array_reference_get_values(array_id, 1, 3)
        .await
        .unwrap();
    assert_eq!(
        values,
        vec![JdwpValue::Int(20), JdwpValue::Int(30), JdwpValue::Int(40)]
    );
}

#[tokio::test]
async fn jdwp_client_supports_classpaths_and_invocation() {
    let server = MockJdwpServer::spawn().await.unwrap();
    let client = JdwpClient::connect(server.addr()).await.unwrap();
    let thread = client.all_threads().await.unwrap()[0];

    let classpaths = client.virtual_machine_class_paths().await.unwrap();
    assert_eq!(classpaths.base_dir, "/mock");
    assert_eq!(classpaths.classpaths, vec!["/mock/classes".to_string()]);
    assert_eq!(classpaths.boot_classpaths, vec!["/mock/boot".to_string()]);

    let loader = client.reference_type_class_loader(0x3001).await.unwrap();
    assert_eq!(loader, 0x8001);

    let defined_class = client
        .class_loader_define_class(loader, "Injected", &[0xCA, 0xFE, 0xBA, 0xBE])
        .await
        .unwrap();
    assert_eq!(defined_class, 0x9001);

    let (value, exception) = client
        .class_type_invoke_method(defined_class, thread, 0x4001, &[JdwpValue::Int(123)], 0)
        .await
        .unwrap();
    assert_eq!(exception, 0);
    assert_eq!(value, JdwpValue::Int(123));

    let arg = JdwpValue::Object {
        tag: b'L',
        id: 0x5001,
    };
    let (value, exception) = client
        .object_reference_invoke_method(0x5001, thread, defined_class, 0x4001, &[arg.clone()], 0)
        .await
        .unwrap();
    assert_eq!(exception, 0);
    assert_eq!(value, arg);
}

#[tokio::test]
async fn jdwp_client_reorders_method_exit_with_return_value_before_stop() {
    let server = MockJdwpServer::spawn().await.unwrap();
    let client = JdwpClient::connect(server.addr()).await.unwrap();
    let mut events = client.subscribe_events();

    let threads = client.all_threads().await.unwrap();
    let thread = threads[0];

    let _step_request = client
        .event_request_set(
            1,
            1,
            vec![EventModifier::Step {
                thread,
                size: 1,
                depth: 1,
            }],
        )
        .await
        .unwrap();
    let _method_exit_request = client
        .event_request_set(42, 0, vec![EventModifier::ThreadOnly { thread }])
        .await
        .unwrap();

    client.vm_resume().await.unwrap();

    let first = events.recv().await.unwrap();
    let second = events.recv().await.unwrap();

    match first {
        JdwpEvent::MethodExitWithReturnValue { value, .. } => {
            assert_eq!(value, JdwpValue::Int(123));
        }
        other => panic!("expected MethodExitWithReturnValue, got {other:?}"),
    }

    match second {
        JdwpEvent::SingleStep { .. } => {}
        other => panic!("expected SingleStep, got {other:?}"),
    }
}

#[tokio::test]
async fn stack_frame_this_object_returns_expected_id() {
    let server = MockJdwpServer::spawn().await.unwrap();
    let client = JdwpClient::connect(server.addr()).await.unwrap();

    let threads = client.all_threads().await.unwrap();
    let thread = threads[0];
    let frames = client.frames(thread, 0, 10).await.unwrap();
    let frame = frames[0];

    let (_argc, vars) = client
        .method_variable_table(frame.location.class_id, frame.location.method_id)
        .await
        .unwrap();
    let slots: Vec<(u32, String)> = vars.iter().map(|v| (v.slot, v.signature.clone())).collect();
    let values = client
        .stack_frame_get_values(thread, frame.frame_id, &slots)
        .await
        .unwrap();
    let expected_object_id = match values[1] {
        JdwpValue::Object { id, .. } => id,
        _ => panic!("expected object value"),
    };

    let this_object = client
        .stack_frame_this_object(thread, frame.frame_id)
        .await
        .unwrap();
    assert_eq!(this_object, expected_object_id);
}

#[tokio::test]
async fn reference_type_get_values_returns_static_values() {
    let server = MockJdwpServer::spawn().await.unwrap();
    let client = JdwpClient::connect(server.addr()).await.unwrap();

    let thread = client.all_threads().await.unwrap()[0];
    let frame = client.frames(thread, 0, 10).await.unwrap()[0];

    let (_argc, vars) = client
        .method_variable_table(frame.location.class_id, frame.location.method_id)
        .await
        .unwrap();
    let slots: Vec<(u32, String)> = vars.iter().map(|v| (v.slot, v.signature.clone())).collect();
    let values = client
        .stack_frame_get_values(thread, frame.frame_id, &slots)
        .await
        .unwrap();

    let object_id = match values[1] {
        JdwpValue::Object { id, .. } => id,
        _ => panic!("expected object value"),
    };
    let string_id = match values[2] {
        JdwpValue::Object { id, .. } => id,
        _ => panic!("expected string object value"),
    };

    // Static-ish primitive field access on the object's reference type.
    let (_ref_type_tag, class_id) = client
        .object_reference_reference_type(object_id)
        .await
        .unwrap();
    let fields = client.reference_type_fields(class_id).await.unwrap();
    let field_ids: Vec<u64> = fields.iter().map(|f| f.field_id).collect();

    let values = client
        .reference_type_get_values(class_id, &field_ids)
        .await
        .unwrap();
    assert_eq!(values, vec![JdwpValue::Int(7)]);

    // Ensure we handle object-like tags in ReferenceType.GetValues replies.
    let throwable = client
        .classes_by_signature("Ljava/lang/Throwable;")
        .await
        .unwrap();
    assert_eq!(throwable.len(), 1);
    let throwable_id = throwable[0].type_id;
    let throwable_fields = client.reference_type_fields(throwable_id).await.unwrap();
    assert_eq!(throwable_fields.len(), 1);

    let msg_values = client
        .reference_type_get_values(throwable_id, &[throwable_fields[0].field_id])
        .await
        .unwrap();
    assert_eq!(
        msg_values,
        vec![JdwpValue::Object {
            tag: b's',
            id: string_id,
        }]
    );
}
