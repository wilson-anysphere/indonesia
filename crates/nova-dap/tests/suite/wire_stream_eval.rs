use nova_dap::wire_stream_eval::define_class_and_invoke_stage0;
use nova_jdwp::wire::mock::DEFINED_STAGE0_METHOD_ID;
use nova_jdwp::wire::types::INVOKE_SINGLE_THREADED;
use nova_jdwp::wire::{mock::MockJdwpServer, JdwpClient, JdwpValue};

#[tokio::test]
async fn wire_stream_eval_define_class_methods_and_invoke_stage0() {
    let server = MockJdwpServer::spawn().await.unwrap();
    let client = JdwpClient::connect(server.addr()).await.unwrap();

    let thread = client.all_threads().await.unwrap()[0];
    let main_class = client.all_classes().await.unwrap()[0].type_id;
    let loader = client
        .reference_type_class_loader(main_class)
        .await
        .unwrap();

    let bytecode = vec![0xCA, 0xFE, 0xBA, 0xBE];
    let value = define_class_and_invoke_stage0(
        &client,
        loader,
        thread,
        "Injected",
        &bytecode,
        &[JdwpValue::Int(42)],
    )
    .await
    .unwrap();
    assert_eq!(value, JdwpValue::Int(42));

    let calls = server.define_class_calls().await;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].loader, loader);
    assert_eq!(calls[0].name, "Injected");
    assert_eq!(calls[0].bytecode_len, bytecode.len());

    let defined_class_id = calls[0].returned_id;

    let methods_calls = server.reference_type_methods_calls().await;
    assert_eq!(methods_calls.len(), 1);
    assert_eq!(methods_calls[0].class_id, defined_class_id);

    let invoke_calls = server.class_type_invoke_method_calls().await;
    assert_eq!(invoke_calls.len(), 1);
    assert_eq!(invoke_calls[0].class_id, defined_class_id);
    assert_eq!(invoke_calls[0].thread, thread);
    assert_eq!(invoke_calls[0].method_id, DEFINED_STAGE0_METHOD_ID);
    assert_eq!(invoke_calls[0].args, vec![JdwpValue::Int(42)]);
    assert_eq!(invoke_calls[0].options, INVOKE_SINGLE_THREADED);
}
