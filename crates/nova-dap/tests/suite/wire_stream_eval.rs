use nova_dap::wire_stream_eval::define_class_and_invoke_stage0;
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
}

