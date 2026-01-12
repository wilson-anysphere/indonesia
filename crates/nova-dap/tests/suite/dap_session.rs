use std::{
    path::PathBuf,
    process::{Command, Stdio},
    time::Duration,
};

use crate::harness::spawn_wire_server;
use base64::{engine::general_purpose, Engine as _};
use nova_dap::object_registry::{OBJECT_HANDLE_BASE, PINNED_SCOPE_REF};
use nova_jdwp::wire::mock::MockJdwpServer;
use nova_test_utils::{env_lock, EnvVarGuard};
use serde_json::json;

fn tool_available(name: &str) -> bool {
    Command::new(name)
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

fn hot_swap_compile_temp_dirs() -> Vec<PathBuf> {
    let base = std::env::temp_dir().join(format!("nova-dap-hot-swap-{}", std::process::id()));
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&base) else {
        return out;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with("compile-") {
            out.push(path);
        }
    }
    out.sort();
    out
}

#[tokio::test]
async fn dap_can_attach_set_breakpoints_and_stop() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();
    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;
    client.attach("127.0.0.1", jdwp.addr().port()).await;

    let bp_resp = client.set_breakpoints("Main.java", &[3]).await;
    let verified = bp_resp
        .pointer("/body/breakpoints/0/verified")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    assert!(verified);
    assert_eq!(jdwp.breakpoint_suspend_policy().await, Some(1));

    let thread_id = client.first_thread_id().await;
    let frame_id = client.first_frame_id(thread_id).await;
    let locals_ref = client.first_scope_variables_reference(frame_id).await;

    let vars_resp = client.variables(locals_ref).await;
    let locals = vars_resp
        .pointer("/body/variables")
        .and_then(|v| v.as_array())
        .unwrap();
    assert!(locals
        .iter()
        .any(|v| v.get("name").and_then(|n| n.as_str()) == Some("x")));

    // Pause should suspend the requested thread and emit a stopped event.
    let (_pause_resp, pause_stopped) = client.pause(Some(thread_id)).await;
    assert_eq!(
        pause_stopped
            .raw
            .pointer("/body/allThreadsStopped")
            .and_then(|v| v.as_bool()),
        Some(false)
    );
    assert_eq!(jdwp.thread_suspend_calls(), 1);
    assert_eq!(jdwp.vm_suspend_calls(), 0);

    // Unknown/unhandled requests should be reported as errors (success: false).
    let bad_resp = client.request("nope", json!({})).await;
    assert_eq!(
        bad_resp.get("success").and_then(|v| v.as_bool()),
        Some(false)
    );

    // Continue should emit a continued event and then a stopped event from the mock JDWP VM.
    let (cont_resp, continued) = client.continue_with_thread_id(Some(thread_id)).await;
    assert_eq!(jdwp.thread_resume_calls(), 1);
    assert_eq!(jdwp.vm_resume_calls(), 0);
    assert_eq!(
        cont_resp
            .pointer("/body/allThreadsContinued")
            .and_then(|v| v.as_bool()),
        Some(false)
    );
    assert_eq!(
        continued
            .pointer("/body/allThreadsContinued")
            .and_then(|v| v.as_bool()),
        Some(false)
    );

    let stopped = client.wait_for_stopped_reason("breakpoint").await;
    // The mock JDWP VM uses SuspendPolicy.EVENT_THREAD (only the event thread is suspended).
    assert_eq!(
        stopped
            .raw
            .pointer("/body/allThreadsStopped")
            .and_then(|v| v.as_bool()),
        Some(false)
    );

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_maps_breakpoints_to_nearest_executable_line() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();
    let (client, server_task) = spawn_wire_server();

    let temp = tempfile::tempdir().unwrap();
    let main_path = temp.path().join("Main.java");
    std::fs::write(
        &main_path,
        "class Main {\n  void main() {\n    int x = 0;\n  }\n}\n",
    )
    .unwrap();

    client.initialize_handshake().await;
    client.attach("127.0.0.1", jdwp.addr().port()).await;

    let bp_resp = client
        .set_breakpoints(main_path.to_str().unwrap(), &[4])
        .await;
    let verified = bp_resp
        .pointer("/body/breakpoints/0/verified")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    assert!(verified);
    assert_eq!(
        bp_resp
            .pointer("/body/breakpoints/0/line")
            .and_then(|v| v.as_i64()),
        Some(3)
    );

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_can_set_variables_in_locals_objects_and_arrays() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();
    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;
    client.attach("127.0.0.1", jdwp.addr().port()).await;
    client.set_breakpoints("Main.java", &[3]).await;

    let thread_id = client.first_thread_id().await;
    let frame_id = client.first_frame_id(thread_id).await;
    let locals_ref = client.first_scope_variables_reference(frame_id).await;

    let vars_resp = client.variables(locals_ref).await;
    let locals = vars_resp
        .pointer("/body/variables")
        .and_then(|v| v.as_array())
        .unwrap();

    let x_value = locals
        .iter()
        .find(|v| v.get("name").and_then(|n| n.as_str()) == Some("x"))
        .and_then(|v| v.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert_eq!(x_value, "42");

    let obj_ref = locals
        .iter()
        .find(|v| v.get("name").and_then(|n| n.as_str()) == Some("obj"))
        .and_then(|v| v.get("variablesReference"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    assert!(obj_ref > OBJECT_HANDLE_BASE);

    let arr_ref = locals
        .iter()
        .find(|v| v.get("name").and_then(|n| n.as_str()) == Some("arr"))
        .and_then(|v| v.get("variablesReference"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    assert!(arr_ref > OBJECT_HANDLE_BASE);

    // Set a local variable.
    let set_local = client
        .request(
            "setVariable",
            json!({
                "variablesReference": locals_ref,
                "name": "x",
                "value": "99",
            }),
        )
        .await;
    assert_eq!(
        set_local.get("success").and_then(|v| v.as_bool()),
        Some(true)
    );

    let stack_calls = jdwp.stack_frame_set_values_calls().await;
    assert_eq!(stack_calls.len(), 1);
    assert!(stack_calls[0]
        .values
        .iter()
        .any(|(slot, value)| *slot == 0 && *value == nova_jdwp::wire::JdwpValue::Int(99)));

    let vars_resp = client.variables(locals_ref).await;
    let locals = vars_resp
        .pointer("/body/variables")
        .and_then(|v| v.as_array())
        .unwrap();
    let x_value = locals
        .iter()
        .find(|v| v.get("name").and_then(|n| n.as_str()) == Some("x"))
        .and_then(|v| v.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert_eq!(x_value, "99");

    // Set an object field.
    let set_field = client
        .request(
            "setVariable",
            json!({
                "variablesReference": obj_ref,
                "name": "field",
                "value": "123",
            }),
        )
        .await;
    assert_eq!(
        set_field.get("success").and_then(|v| v.as_bool()),
        Some(true)
    );

    let object_calls = jdwp.object_reference_set_values_calls().await;
    assert_eq!(object_calls.len(), 1);
    assert!(object_calls[0]
        .values
        .iter()
        .any(|(_field_id, value)| *value == nova_jdwp::wire::JdwpValue::Int(123)));

    let obj_vars = client.variables(obj_ref).await;
    let obj_children = obj_vars
        .pointer("/body/variables")
        .and_then(|v| v.as_array())
        .unwrap();
    let field_value = obj_children
        .iter()
        .find(|v| v.get("name").and_then(|n| n.as_str()) == Some("field"))
        .and_then(|v| v.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert_eq!(field_value, "123");

    // Set an array element.
    let set_arr = client
        .request(
            "setVariable",
            json!({
                "variablesReference": arr_ref,
                "name": "[1]",
                "value": "999",
            }),
        )
        .await;
    assert_eq!(set_arr.get("success").and_then(|v| v.as_bool()), Some(true));

    let array_calls = jdwp.array_reference_set_values_calls().await;
    assert_eq!(array_calls.len(), 1);
    assert_eq!(array_calls[0].first_index, 1);
    assert_eq!(
        array_calls[0].values,
        vec![nova_jdwp::wire::JdwpValue::Int(999)]
    );

    let arr_vars = client.variables(arr_ref).await;
    let arr_children = arr_vars
        .pointer("/body/variables")
        .and_then(|v| v.as_array())
        .unwrap();
    let idx_value = arr_children
        .iter()
        .find(|v| v.get("name").and_then(|n| n.as_str()) == Some("[1]"))
        .and_then(|v| v.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert_eq!(idx_value, "999");

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_locals_include_this_object() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();
    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;
    client.attach("127.0.0.1", jdwp.addr().port()).await;
    client.set_breakpoints("Main.java", &[3]).await;

    let thread_id = client.first_thread_id().await;
    let frame_id = client.first_frame_id(thread_id).await;
    let locals_ref = client.first_scope_variables_reference(frame_id).await;

    let vars_resp = client.variables(locals_ref).await;
    let locals = vars_resp
        .pointer("/body/variables")
        .and_then(|v| v.as_array())
        .unwrap();

    let this_ref = locals
        .iter()
        .find(|v| v.get("name").and_then(|n| n.as_str()) == Some("this"))
        .and_then(|v| v.get("variablesReference"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    assert!(this_ref > OBJECT_HANDLE_BASE);

    let this_vars = client.variables(this_ref).await;
    let children = this_vars
        .pointer("/body/variables")
        .and_then(|v| v.as_array())
        .unwrap();
    let field_value = children
        .iter()
        .find(|v| v.get("name").and_then(|n| n.as_str()) == Some("field"))
        .and_then(|v| v.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert_eq!(field_value, "7");

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_static_scope_can_read_and_set_static_fields() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();
    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;
    client.attach("127.0.0.1", jdwp.addr().port()).await;

    let bp_resp = client.set_breakpoints("Main.java", &[3]).await;
    assert!(bp_resp
        .pointer("/body/breakpoints/0/verified")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

    let thread_id = client.first_thread_id().await;
    client.continue_with_thread_id(Some(thread_id)).await;
    let _ = client.wait_for_stopped_reason("breakpoint").await;

    let frame_id = client.first_frame_id(thread_id).await;
    let scopes = client
        .request("scopes", json!({ "frameId": frame_id }))
        .await;
    assert_eq!(scopes.get("success").and_then(|v| v.as_bool()), Some(true));

    let scopes_arr = scopes
        .pointer("/body/scopes")
        .and_then(|v| v.as_array())
        .unwrap();
    let static_ref = scopes_arr
        .iter()
        .find(|scope| scope.get("name").and_then(|v| v.as_str()) == Some("Static"))
        .and_then(|scope| scope.get("variablesReference"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    assert!(static_ref > 0);

    let static_vars = client.variables(static_ref).await;
    let vars = static_vars
        .pointer("/body/variables")
        .and_then(|v| v.as_array())
        .unwrap();
    let initial = vars
        .iter()
        .find(|v| v.get("name").and_then(|n| n.as_str()) == Some("staticField"))
        .and_then(|v| v.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert_eq!(initial, "0");

    let set_resp = client
        .request(
            "setVariable",
            json!({
                "variablesReference": static_ref,
                "name": "staticField",
                "value": "123",
            }),
        )
        .await;
    assert_eq!(
        set_resp.get("success").and_then(|v| v.as_bool()),
        Some(true)
    );

    let calls = jdwp.class_type_set_values_calls().await;
    assert_eq!(calls.len(), 1);

    let static_vars = client.variables(static_ref).await;
    let vars = static_vars
        .pointer("/body/variables")
        .and_then(|v| v.as_array())
        .unwrap();
    let updated = vars
        .iter()
        .find(|v| v.get("name").and_then(|n| n.as_str()) == Some("staticField"))
        .and_then(|v| v.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert_eq!(updated, "123");

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_step_in_targets_lists_calls_on_current_line() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();
    let (client, server_task) = spawn_wire_server();

    let temp = tempfile::tempdir().unwrap();
    let main_path = temp.path().join("Main.java");
    std::fs::write(
        &main_path,
        "class Main {\n  void main() {\n    foo(bar(), baz(qux()), corge()).trim();\n  }\n}\n",
    )
    .unwrap();

    client.initialize_handshake().await;
    client.attach("127.0.0.1", jdwp.addr().port()).await;

    let bp_resp = client
        .set_breakpoints(main_path.to_str().unwrap(), &[3])
        .await;
    assert!(bp_resp
        .pointer("/body/breakpoints/0/verified")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

    let thread_id = client.first_thread_id().await;
    client.continue_with_thread_id(Some(thread_id)).await;
    let _ = client.wait_for_stopped_reason("breakpoint").await;

    let frame_id = client.first_frame_id(thread_id).await;
    let resp = client
        .request("stepInTargets", json!({ "frameId": frame_id }))
        .await;
    assert_eq!(resp.get("success").and_then(|v| v.as_bool()), Some(true));

    let targets = resp
        .pointer("/body/targets")
        .and_then(|v| v.as_array())
        .unwrap();
    let labels: Vec<_> = targets
        .iter()
        .filter_map(|t| t.get("label").and_then(|v| v.as_str()))
        .collect();
    assert_eq!(
        labels,
        vec!["bar()", "qux()", "baz()", "corge()", "foo()", "trim()"]
    );

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_step_in_target_honors_target_id() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();
    let (client, server_task) = spawn_wire_server();

    let temp = tempfile::tempdir().unwrap();
    let main_path = temp.path().join("Main.java");
    std::fs::write(
        &main_path,
        "class Main {\n  void main() {\n    foo(bar(), baz(qux()), corge()).trim();\n  }\n}\n",
    )
    .unwrap();

    client.initialize_handshake().await;
    client.attach("127.0.0.1", jdwp.addr().port()).await;
    client
        .set_breakpoints(main_path.to_str().unwrap(), &[3])
        .await;

    let thread_id = client.first_thread_id().await;
    client.continue_with_thread_id(Some(thread_id)).await;
    let _ = client.wait_for_stopped_reason("breakpoint").await;

    let frame_id = client.first_frame_id(thread_id).await;
    let targets_resp = client
        .request("stepInTargets", json!({ "frameId": frame_id }))
        .await;
    assert_eq!(
        targets_resp.get("success").and_then(|v| v.as_bool()),
        Some(true)
    );

    let targets = targets_resp
        .pointer("/body/targets")
        .and_then(|v| v.as_array())
        .unwrap();
    let baz_id = targets
        .iter()
        .find(|t| t.get("label").and_then(|v| v.as_str()) == Some("baz()"))
        .and_then(|t| t.get("id"))
        .and_then(|v| v.as_i64())
        .unwrap();

    let resumes_before = jdwp.thread_resume_calls();
    let step_resp = client
        .request(
            "stepIn",
            json!({ "threadId": thread_id, "targetId": baz_id }),
        )
        .await;
    assert_eq!(
        step_resp.get("success").and_then(|v| v.as_bool()),
        Some(true)
    );

    let stopped = client.wait_for_stopped_reason("step").await;
    assert_eq!(stopped.thread_id, Some(thread_id));

    let resumes_after = jdwp.thread_resume_calls();
    assert_eq!(resumes_after - resumes_before, 5);

    let stack = client
        .request("stackTrace", json!({ "threadId": thread_id }))
        .await;
    let top_name = stack
        .pointer("/body/stackFrames/0/name")
        .and_then(|v| v.as_str());
    assert_eq!(top_name, Some("baz"));

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_can_hot_swap_a_class() {
    let mut caps = vec![false; 32];
    caps[7] = true; // canRedefineClasses
    let jdwp = MockJdwpServer::spawn_with_capabilities(caps).await.unwrap();

    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;
    client.attach("127.0.0.1", jdwp.addr().port()).await;

    let bytecode = vec![0xCA, 0xFE];
    let bytecode_base64 = general_purpose::STANDARD.encode(&bytecode);
    let hot_swap_resp = client
        .request(
            "nova/hotSwap",
            json!({
                "classes": [{
                    "className": "Main",
                    "bytecodeBase64": bytecode_base64,
                }]
            }),
        )
        .await;

    assert_eq!(
        hot_swap_resp.get("success").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        hot_swap_resp
            .pointer("/body/results/0/status")
            .and_then(|v| v.as_str()),
        Some("success")
    );
    assert_eq!(
        hot_swap_resp
            .pointer("/body/results/0/file")
            .and_then(|v| v.as_str()),
        Some("Main.java")
    );
    assert_eq!(
        hot_swap_resp
            .pointer("/body/results/0/classes/0/className")
            .and_then(|v| v.as_str()),
        Some("Main")
    );
    assert_eq!(
        hot_swap_resp
            .pointer("/body/results/0/classes/0/status")
            .and_then(|v| v.as_str()),
        Some("success")
    );

    let calls = jdwp.redefine_classes_calls().await;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].class_count, 1);
    assert_eq!(calls[0].classes.len(), 1);
    assert_eq!(calls[0].classes[0].1, bytecode);

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_can_hot_swap_multiple_classes_per_file() {
    let mut caps = vec![false; 32];
    caps[7] = true; // canRedefineClasses
    let jdwp = MockJdwpServer::spawn_with_capabilities(caps).await.unwrap();

    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;
    client.attach("127.0.0.1", jdwp.addr().port()).await;

    let bytecode_main = vec![0xCA, 0xFE];
    let bytecode_foo = vec![0xBE, 0xEF];

    let hot_swap_resp = client
        .request(
            "nova/hotSwap",
            json!({
                "classes": [
                    {
                        "className": "Main",
                        "bytecodeBase64": general_purpose::STANDARD.encode(&bytecode_main),
                        "file": "Main.java",
                    },
                    {
                        "className": "com.example.Foo",
                        "bytecodeBase64": general_purpose::STANDARD.encode(&bytecode_foo),
                        "file": "Main.java",
                    }
                ]
            }),
        )
        .await;

    assert_eq!(
        hot_swap_resp.get("success").and_then(|v| v.as_bool()),
        Some(true)
    );

    let results = hot_swap_resp
        .pointer("/body/results")
        .and_then(|v| v.as_array())
        .expect("expected hot swap results");
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].get("file").and_then(|v| v.as_str()),
        Some("Main.java")
    );
    assert_eq!(
        results[0].get("status").and_then(|v| v.as_str()),
        Some("success")
    );

    let classes = results[0]
        .get("classes")
        .and_then(|v| v.as_array())
        .expect("expected classes array");
    assert_eq!(classes.len(), 2);
    assert!(classes
        .iter()
        .all(|entry| entry.get("status").and_then(|v| v.as_str()) == Some("success")));

    let names: std::collections::BTreeSet<_> = classes
        .iter()
        .filter_map(|entry| entry.get("className").and_then(|v| v.as_str()))
        .collect();
    assert!(names.contains("Main"));
    assert!(names.contains("com.example.Foo"));

    let calls = jdwp.redefine_classes_calls().await;
    let all_bytecodes: Vec<Vec<u8>> = calls
        .iter()
        .flat_map(|call| call.classes.iter().map(|(_, bytes)| bytes.clone()))
        .collect();
    assert_eq!(all_bytecodes.len(), 2);
    assert!(all_bytecodes.contains(&bytecode_main));
    assert!(all_bytecodes.contains(&bytecode_foo));

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

pub(super) async fn dap_hot_swap_can_compile_changed_files_with_javac() {
    if !tool_available("javac") {
        // CI images may not include a JDK; skip rather than failing.
        return;
    }

    // Ensure the test environment is deterministic even if a previous run left
    // behind temp directories.
    let _env_lock = env_lock();
    let _keep_temp_guard = EnvVarGuard::unset("NOVA_DAP_KEEP_HOT_SWAP_TEMP");
    let hot_swap_base =
        std::env::temp_dir().join(format!("nova-dap-hot-swap-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(hot_swap_base);

    let mut caps = vec![false; 32];
    caps[7] = true; // canRedefineClasses
    let jdwp = MockJdwpServer::spawn_with_capabilities(caps).await.unwrap();

    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;
    client.attach("127.0.0.1", jdwp.addr().port()).await;

    let fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join("java")
        .join("Main.java");
    assert!(fixture_path.is_file());

    let hot_swap_resp = client
        .request(
            "nova/hotSwap",
            json!({
                "changedFiles": [fixture_path],
            }),
        )
        .await;

    assert_eq!(
        hot_swap_resp.get("success").and_then(|v| v.as_bool()),
        Some(true),
        "expected hot swap to succeed: {hot_swap_resp}"
    );
    assert_eq!(
        hot_swap_resp
            .pointer("/body/results/0/status")
            .and_then(|v| v.as_str()),
        Some("success"),
        "unexpected hot swap result: {hot_swap_resp}"
    );

    let calls = jdwp.redefine_classes_calls().await;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].class_count, 1);
    assert_eq!(calls[0].classes.len(), 1);
    let bytes = &calls[0].classes[0].1;
    assert!(
        bytes.starts_with(&[0xCA, 0xFE, 0xBA, 0xBE]),
        "expected classfile magic, got {:?}",
        bytes.get(0..4)
    );

    client.disconnect().await;
    server_task.await.unwrap().unwrap();

    // Tests run in parallel; give other hot-swap/stream-eval compilation paths a moment to drop
    // their temp dirs before asserting on global filesystem state.
    let mut leaked = Vec::new();
    for _ in 0..100 {
        leaked = hot_swap_compile_temp_dirs();
        if leaked.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        leaked.is_empty(),
        "expected hot-swap compilation temp dirs to be cleaned up, found: {leaked:?}"
    );
}

#[tokio::test]
async fn dap_can_hot_swap_multiple_classes_from_single_file() {
    let mut caps = vec![false; 32];
    caps[7] = true; // canRedefineClasses
    let jdwp = MockJdwpServer::spawn_with_capabilities(caps).await.unwrap();

    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;
    client.attach("127.0.0.1", jdwp.addr().port()).await;

    let outer_bytecode = vec![0xCA, 0xFE];
    let inner_bytecode = vec![0xBE, 0xEF];

    let hot_swap_resp = client
        .request(
            "nova/hotSwap",
            json!({
                "classes": [
                    {
                        "className": "Main",
                        "bytecodeBase64": general_purpose::STANDARD.encode(&outer_bytecode),
                    },
                    {
                        "className": "Main$Inner",
                        "bytecodeBase64": general_purpose::STANDARD.encode(&inner_bytecode),
                    }
                ]
            }),
        )
        .await;

    assert_eq!(
        hot_swap_resp.get("success").and_then(|v| v.as_bool()),
        Some(true)
    );

    let results = hot_swap_resp
        .pointer("/body/results")
        .and_then(|v| v.as_array())
        .expect("results array");
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].get("status").and_then(|v| v.as_str()),
        Some("success")
    );
    assert_eq!(
        results[0].get("file").and_then(|v| v.as_str()),
        Some("Main.java")
    );

    let calls = jdwp.redefine_classes_calls().await;
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].class_count, 1);
    assert_eq!(calls[0].classes[0].1, outer_bytecode);
    assert_eq!(calls[1].class_count, 1);
    assert_eq!(calls[1].classes[0].1, inner_bytecode);

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_hot_swap_reports_schema_change() {
    let mut caps = vec![false; 32];
    caps[7] = true; // canRedefineClasses
    let jdwp = MockJdwpServer::spawn_with_capabilities(caps).await.unwrap();
    jdwp.set_redefine_classes_error_code(62); // SCHEMA_CHANGE_NOT_IMPLEMENTED

    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;
    client.attach("127.0.0.1", jdwp.addr().port()).await;

    let bytecode_base64 = general_purpose::STANDARD.encode([0u8; 4]);
    let hot_swap_resp = client
        .request(
            "nova/hotSwap",
            json!({
                "classes": [{
                    "className": "Main",
                    "bytecodeBase64": bytecode_base64,
                }]
            }),
        )
        .await;

    assert_eq!(
        hot_swap_resp.get("success").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        hot_swap_resp
            .pointer("/body/results/0/status")
            .and_then(|v| v.as_str()),
        Some("schema_change")
    );
    let msg = hot_swap_resp
        .pointer("/body/results/0/message")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(msg.contains("JDWP error 62"), "unexpected message: {msg:?}");

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_wire_handle_tables_are_stable_within_stop_and_invalidated_on_resume() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();
    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;
    client.attach("127.0.0.1", jdwp.addr().port()).await;

    let bp_resp = client.set_breakpoints("Main.java", &[3]).await;
    assert!(bp_resp
        .pointer("/body/breakpoints/0/verified")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));
    assert_eq!(jdwp.breakpoint_suspend_policy().await, Some(1));

    let thread_id = client.first_thread_id().await;

    // Continue to generate an initial stop.
    client.continue_with_thread_id(Some(thread_id)).await;
    let stopped = client.wait_for_stopped_reason("breakpoint").await;
    assert_eq!(
        stopped
            .raw
            .pointer("/body/allThreadsStopped")
            .and_then(|v| v.as_bool()),
        Some(false)
    );

    // Repeated stackTrace calls should return stable frame ids.
    let stack_a = client
        .request("stackTrace", json!({ "threadId": thread_id }))
        .await;
    let frame_id_a = stack_a
        .pointer("/body/stackFrames/0/id")
        .and_then(|v| v.as_i64())
        .unwrap();
    let stack_b = client
        .request("stackTrace", json!({ "threadId": thread_id }))
        .await;
    let frame_id_b = stack_b
        .pointer("/body/stackFrames/0/id")
        .and_then(|v| v.as_i64())
        .unwrap();
    assert_eq!(frame_id_a, frame_id_b);

    // And repeated scopes calls should return stable locals handles.
    let scopes_a = client
        .request("scopes", json!({ "frameId": frame_id_a }))
        .await;
    let locals_ref_a = scopes_a
        .pointer("/body/scopes/0/variablesReference")
        .and_then(|v| v.as_i64())
        .unwrap();
    let scopes_b = client
        .request("scopes", json!({ "frameId": frame_id_a }))
        .await;
    let locals_ref_b = scopes_b
        .pointer("/body/scopes/0/variablesReference")
        .and_then(|v| v.as_i64())
        .unwrap();
    assert_eq!(locals_ref_a, locals_ref_b);

    // Resume; the next stop should allocate fresh handles (stale ids must not alias).
    client.continue_with_thread_id(Some(thread_id)).await;
    let _ = client.wait_for_stopped_reason("breakpoint").await;

    let stack_after = client
        .request("stackTrace", json!({ "threadId": thread_id }))
        .await;
    let frame_id_after = stack_after
        .pointer("/body/stackFrames/0/id")
        .and_then(|v| v.as_i64())
        .unwrap();
    assert_ne!(frame_id_a, frame_id_after);

    let scopes_after = client
        .request("scopes", json!({ "frameId": frame_id_after }))
        .await;
    let locals_ref_after = scopes_after
        .pointer("/body/scopes/0/variablesReference")
        .and_then(|v| v.as_i64())
        .unwrap();
    assert_ne!(locals_ref_a, locals_ref_after);

    // Old frame ids should be rejected rather than resolving to a different frame.
    let stale_scopes = client
        .request("scopes", json!({ "frameId": frame_id_a }))
        .await;
    assert_eq!(
        stale_scopes.get("success").and_then(|v| v.as_bool()),
        Some(false)
    );

    // Old variables references should return empty results.
    let stale_vars = client
        .request("variables", json!({ "variablesReference": locals_ref_a }))
        .await;
    let vars = stale_vars
        .pointer("/body/variables")
        .and_then(|v| v.as_array())
        .unwrap();
    assert!(vars.is_empty());

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_wire_handle_tables_are_invalidated_on_step_in_target() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();
    let (client, server_task) = spawn_wire_server();

    let temp = tempfile::tempdir().unwrap();
    let main_path = temp.path().join("Main.java");
    std::fs::write(
        &main_path,
        "class Main {\n  void main() {\n    foo(bar(), baz(qux()), corge()).trim();\n  }\n}\n",
    )
    .unwrap();

    client.initialize_handshake().await;
    client.attach("127.0.0.1", jdwp.addr().port()).await;
    client
        .set_breakpoints(main_path.to_str().unwrap(), &[3])
        .await;

    let thread_id = client.first_thread_id().await;

    client.continue_with_thread_id(Some(thread_id)).await;
    let _ = client.wait_for_stopped_reason("breakpoint").await;

    // Repeated stackTrace calls should return stable frame ids.
    let stack_a = client
        .request("stackTrace", json!({ "threadId": thread_id }))
        .await;
    let frame_id_a = stack_a
        .pointer("/body/stackFrames/0/id")
        .and_then(|v| v.as_i64())
        .unwrap();
    let stack_b = client
        .request("stackTrace", json!({ "threadId": thread_id }))
        .await;
    let frame_id_b = stack_b
        .pointer("/body/stackFrames/0/id")
        .and_then(|v| v.as_i64())
        .unwrap();
    assert_eq!(frame_id_a, frame_id_b);

    // And repeated scopes calls should return stable locals handles.
    let scopes_a = client
        .request("scopes", json!({ "frameId": frame_id_a }))
        .await;
    let locals_ref_a = scopes_a
        .pointer("/body/scopes/0/variablesReference")
        .and_then(|v| v.as_i64())
        .unwrap();
    let scopes_b = client
        .request("scopes", json!({ "frameId": frame_id_a }))
        .await;
    let locals_ref_b = scopes_b
        .pointer("/body/scopes/0/variablesReference")
        .and_then(|v| v.as_i64())
        .unwrap();
    assert_eq!(locals_ref_a, locals_ref_b);

    // Step via targetId; the next stop should allocate fresh handles (stale ids must not alias).
    let targets_resp = client
        .request("stepInTargets", json!({ "frameId": frame_id_a }))
        .await;
    assert_eq!(
        targets_resp.get("success").and_then(|v| v.as_bool()),
        Some(true)
    );
    let targets = targets_resp
        .pointer("/body/targets")
        .and_then(|v| v.as_array())
        .unwrap();
    let baz_id = targets
        .iter()
        .find(|t| t.get("label").and_then(|v| v.as_str()) == Some("baz()"))
        .and_then(|t| t.get("id"))
        .and_then(|v| v.as_i64())
        .unwrap();

    let step_resp = client
        .request(
            "stepIn",
            json!({ "threadId": thread_id, "targetId": baz_id }),
        )
        .await;
    assert_eq!(
        step_resp.get("success").and_then(|v| v.as_bool()),
        Some(true)
    );
    let _ = client.wait_for_stopped_reason("step").await;

    let stack_after = client
        .request("stackTrace", json!({ "threadId": thread_id }))
        .await;
    let frame_id_after = stack_after
        .pointer("/body/stackFrames/0/id")
        .and_then(|v| v.as_i64())
        .unwrap();
    assert_ne!(frame_id_a, frame_id_after);

    let scopes_after = client
        .request("scopes", json!({ "frameId": frame_id_after }))
        .await;
    let locals_ref_after = scopes_after
        .pointer("/body/scopes/0/variablesReference")
        .and_then(|v| v.as_i64())
        .unwrap();
    assert_ne!(locals_ref_a, locals_ref_after);

    // Old frame ids should be rejected rather than resolving to a different frame.
    let stale_scopes = client
        .request("scopes", json!({ "frameId": frame_id_a }))
        .await;
    assert_eq!(
        stale_scopes.get("success").and_then(|v| v.as_bool()),
        Some(false)
    );

    // Old variables references should return empty results.
    let stale_vars = client
        .request("variables", json!({ "variablesReference": locals_ref_a }))
        .await;
    let vars = stale_vars
        .pointer("/body/variables")
        .and_then(|v| v.as_array())
        .unwrap();
    assert!(vars.is_empty());

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_object_handles_are_stable_across_stops_and_pinning_exposes_them_in_a_scope() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();
    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;
    client.attach("127.0.0.1", jdwp.addr().port()).await;

    let bp_resp = client.set_breakpoints("Main.java", &[3]).await;
    assert!(bp_resp
        .pointer("/body/breakpoints/0/verified")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

    let thread_id = client.first_thread_id().await;

    // First stop.
    client.continue_with_thread_id(Some(thread_id)).await;
    let _ = client.wait_for_stopped_reason("breakpoint").await;

    let frame_id = client.first_frame_id(thread_id).await;
    let scopes_resp = client
        .request("scopes", json!({ "frameId": frame_id }))
        .await;
    let locals_ref = scopes_resp
        .pointer("/body/scopes/0/variablesReference")
        .and_then(|v| v.as_i64())
        .unwrap();

    let vars_resp = client
        .request("variables", json!({ "variablesReference": locals_ref }))
        .await;
    let locals = vars_resp
        .pointer("/body/variables")
        .and_then(|v| v.as_array())
        .unwrap();
    let obj_ref = locals
        .iter()
        .find(|v| v.get("name").and_then(|n| n.as_str()) == Some("obj"))
        .and_then(|v| v.get("variablesReference"))
        .and_then(|v| v.as_i64())
        .unwrap();
    assert!(obj_ref > OBJECT_HANDLE_BASE);

    // Not pinned: object handles should remain stable across resumes as long as the
    // underlying object is still alive.
    client.continue_with_thread_id(Some(thread_id)).await;
    let _ = client.wait_for_stopped_reason("breakpoint").await;

    let stale_obj_vars = client
        .request("variables", json!({ "variablesReference": obj_ref }))
        .await;
    let stale = stale_obj_vars
        .pointer("/body/variables")
        .and_then(|v| v.as_array())
        .unwrap();
    assert!(!stale.is_empty());

    // Pin a fresh object handle.
    let frame_id = client.first_frame_id(thread_id).await;
    let scopes_resp = client
        .request("scopes", json!({ "frameId": frame_id }))
        .await;
    let locals_ref = scopes_resp
        .pointer("/body/scopes/0/variablesReference")
        .and_then(|v| v.as_i64())
        .unwrap();
    let pinned_ref = scopes_resp
        .pointer("/body/scopes/1/variablesReference")
        .and_then(|v| v.as_i64())
        .unwrap();
    assert_eq!(pinned_ref, PINNED_SCOPE_REF);

    let vars_resp = client
        .request("variables", json!({ "variablesReference": locals_ref }))
        .await;
    let locals = vars_resp
        .pointer("/body/variables")
        .and_then(|v| v.as_array())
        .unwrap();
    let obj_ref = locals
        .iter()
        .find(|v| v.get("name").and_then(|n| n.as_str()) == Some("obj"))
        .and_then(|v| v.get("variablesReference"))
        .and_then(|v| v.as_i64())
        .unwrap();

    let pin_resp = client
        .request(
            "nova/pinObject",
            json!({ "variablesReference": obj_ref, "pinned": true }),
        )
        .await;
    assert_eq!(
        pin_resp.pointer("/body/pinned").and_then(|v| v.as_bool()),
        Some(true)
    );

    // Resume again; pinned handle must survive.
    client.continue_with_thread_id(Some(thread_id)).await;
    let _ = client.wait_for_stopped_reason("breakpoint").await;

    let pinned_vars_resp = client
        .request(
            "variables",
            json!({ "variablesReference": PINNED_SCOPE_REF }),
        )
        .await;
    let pinned_vars = pinned_vars_resp
        .pointer("/body/variables")
        .and_then(|v| v.as_array())
        .unwrap();
    assert!(pinned_vars
        .iter()
        .any(|v| v.get("variablesReference").and_then(|v| v.as_i64()) == Some(obj_ref)));

    let obj_vars = client
        .request("variables", json!({ "variablesReference": obj_ref }))
        .await;
    let fields = obj_vars
        .pointer("/body/variables")
        .and_then(|v| v.as_array())
        .unwrap();
    assert!(!fields.is_empty());

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_step_stop_uses_event_thread_suspend_policy() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();
    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;
    client.attach("127.0.0.1", jdwp.addr().port()).await;

    let thread_id = client.first_thread_id().await;
    client.next(thread_id).await;
    let stopped = client.wait_for_stopped_reason("step").await;

    assert_eq!(jdwp.thread_resume_calls(), 1);
    assert_eq!(jdwp.vm_resume_calls(), 0);
    assert_eq!(jdwp.step_suspend_policy().await, Some(1));
    assert_eq!(
        stopped
            .raw
            .pointer("/body/allThreadsStopped")
            .and_then(|v| v.as_bool()),
        Some(false)
    );

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_can_expand_object_fields_and_pin_objects() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();
    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;
    client.attach("127.0.0.1", jdwp.addr().port()).await;

    let thread_id = client.first_thread_id().await;
    let frame_id = client.first_frame_id(thread_id).await;

    let scopes_resp = client
        .request("scopes", json!({ "frameId": frame_id }))
        .await;
    let locals_ref = scopes_resp
        .pointer("/body/scopes/0/variablesReference")
        .and_then(|v| v.as_i64())
        .unwrap();
    let pinned_ref = scopes_resp
        .pointer("/body/scopes/1/variablesReference")
        .and_then(|v| v.as_i64())
        .unwrap();
    assert_eq!(pinned_ref, PINNED_SCOPE_REF);

    let vars_resp = client
        .request("variables", json!({ "variablesReference": locals_ref }))
        .await;
    let locals = vars_resp
        .pointer("/body/variables")
        .and_then(|v| v.as_array())
        .unwrap();

    let obj = locals
        .iter()
        .find(|v| v.get("name").and_then(|n| n.as_str()) == Some("obj"))
        .expect("expected locals to contain obj");
    let obj_ref = obj
        .get("variablesReference")
        .and_then(|v| v.as_i64())
        .unwrap();
    assert!(
        obj_ref > OBJECT_HANDLE_BASE,
        "expected stable object handle variablesReference"
    );
    assert!(obj
        .get("value")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .contains('@'));

    let fields_resp = client
        .request("variables", json!({ "variablesReference": obj_ref }))
        .await;
    let fields = fields_resp
        .pointer("/body/variables")
        .and_then(|v| v.as_array())
        .unwrap();
    let field = fields
        .iter()
        .find(|v| v.get("name").and_then(|n| n.as_str()) == Some("field"))
        .expect("expected object to contain field");
    assert_eq!(field.get("value").and_then(|v| v.as_str()), Some("7"));
    assert_eq!(field.get("type").and_then(|v| v.as_str()), Some("int"));

    // Pin the object.
    let pin_resp = client
        .request(
            "nova/pinObject",
            json!({ "variablesReference": obj_ref, "pinned": true }),
        )
        .await;
    assert_eq!(
        pin_resp.pointer("/body/pinned").and_then(|v| v.as_bool()),
        Some(true)
    );

    // Pinned objects are visible under the synthetic scope.
    let pinned_vars_resp = client
        .request(
            "variables",
            json!({ "variablesReference": PINNED_SCOPE_REF }),
        )
        .await;
    let pinned_vars = pinned_vars_resp
        .pointer("/body/variables")
        .and_then(|v| v.as_array())
        .unwrap();
    assert_eq!(pinned_vars.len(), 1);
    assert_eq!(
        pinned_vars[0]
            .get("variablesReference")
            .and_then(|v| v.as_i64()),
        Some(obj_ref)
    );
    assert_eq!(
        pinned_vars[0]
            .get("presentationHint")
            .and_then(|v| v.get("attributes"))
            .and_then(|v| v.get(0))
            .and_then(|v| v.as_str()),
        Some("pinned")
    );

    // Unpin the object.
    let unpin_resp = client
        .request(
            "nova/pinObject",
            json!({ "variablesReference": obj_ref, "pinned": false }),
        )
        .await;
    assert_eq!(
        unpin_resp.pointer("/body/pinned").and_then(|v| v.as_bool()),
        Some(false)
    );

    let pinned_empty_resp = client
        .request(
            "variables",
            json!({ "variablesReference": PINNED_SCOPE_REF }),
        )
        .await;
    let pinned_empty = pinned_empty_resp
        .pointer("/body/variables")
        .and_then(|v| v.as_array())
        .unwrap();
    assert!(pinned_empty.is_empty());

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_exception_info_includes_type_name() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();
    let (client, server_task) = spawn_wire_server();

    let init_resp = client.initialize_handshake().await;
    assert_eq!(
        init_resp
            .pointer("/body/supportsExceptionInfoRequest")
            .and_then(|v| v.as_bool()),
        Some(true)
    );

    client.attach("127.0.0.1", jdwp.addr().port()).await;

    let exc_bp_resp = client
        .request("setExceptionBreakpoints", json!({ "filters": ["all"] }))
        .await;
    assert_eq!(
        exc_bp_resp.get("success").and_then(|v| v.as_bool()),
        Some(true)
    );

    let thread_id = client.first_thread_id().await;

    client.continue_with_thread_id(Some(thread_id)).await;
    let stopped = client.wait_for_stopped_reason("exception").await;
    assert_eq!(stopped.reason.as_deref(), Some("exception"));

    let exc_info = client
        .request("exceptionInfo", json!({ "threadId": thread_id }))
        .await;
    assert_eq!(
        exc_info.get("success").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        exc_info
            .pointer("/body/exceptionId")
            .and_then(|v| v.as_str()),
        Some("java.lang.RuntimeException")
    );
    assert_eq!(
        exc_info
            .pointer("/body/details/fullTypeName")
            .and_then(|v| v.as_str()),
        Some("java.lang.RuntimeException")
    );
    assert_eq!(
        exc_info
            .pointer("/body/details/typeName")
            .and_then(|v| v.as_str()),
        Some("RuntimeException")
    );
    assert_eq!(
        exc_info.pointer("/body/breakMode").and_then(|v| v.as_str()),
        Some("always")
    );
    assert_eq!(
        exc_info
            .pointer("/body/description")
            .and_then(|v| v.as_str()),
        Some("mock string")
    );
    assert_eq!(
        exc_info
            .pointer("/body/details/message")
            .and_then(|v| v.as_str()),
        Some("mock string")
    );

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_emits_thread_start_and_death_events() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();
    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;
    client.attach("127.0.0.1", jdwp.addr().port()).await;

    // Trigger the mock VM to emit thread lifecycle events.
    client.continue_().await;

    let started = client
        .wait_for_event_matching("thread(started)", Duration::from_secs(2), |msg| {
            msg.get("type").and_then(|v| v.as_str()) == Some("event")
                && msg.get("event").and_then(|v| v.as_str()) == Some("thread")
                && msg.pointer("/body/reason").and_then(|v| v.as_str()) == Some("started")
        })
        .await;
    let exited = client
        .wait_for_event_matching("thread(exited)", Duration::from_secs(2), |msg| {
            msg.get("type").and_then(|v| v.as_str()) == Some("event")
                && msg.get("event").and_then(|v| v.as_str()) == Some("thread")
                && msg.pointer("/body/reason").and_then(|v| v.as_str()) == Some("exited")
        })
        .await;

    let started_thread_id = started.pointer("/body/threadId").and_then(|v| v.as_i64());
    let exited_thread_id = exited.pointer("/body/threadId").and_then(|v| v.as_i64());
    assert_eq!(started_thread_id, exited_thread_id);
    assert_eq!(jdwp.vm_resume_calls(), 1);
    assert_eq!(jdwp.thread_resume_calls(), 0);

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_feature_requests_are_guarded_by_jdwp_capabilities() {
    // Mock VM reports all capabilities as `false` by default.
    let jdwp = MockJdwpServer::spawn().await.unwrap();
    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;
    client.attach("127.0.0.1", jdwp.addr().port()).await;

    // Watchpoints / data breakpoints are gated by canWatchField* capabilities.
    let watch_resp = client.request("dataBreakpointInfo", json!({})).await;
    assert_eq!(
        watch_resp.get("success").and_then(|v| v.as_bool()),
        Some(false)
    );
    let watch_msg = watch_resp
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(watch_msg.contains("canWatchFieldModification"));

    // Hot swap is gated by canRedefineClasses.
    let hot_swap_resp = client.request("redefineClasses", json!({})).await;
    assert_eq!(
        hot_swap_resp.get("success").and_then(|v| v.as_bool()),
        Some(false)
    );
    let hot_swap_msg = hot_swap_resp
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(hot_swap_msg.contains("canRedefineClasses"));

    // Method return values are gated by canGetMethodReturnValues.
    let ret_resp = client
        .request("nova/enableMethodReturnValues", json!({}))
        .await;
    assert_eq!(
        ret_resp.get("success").and_then(|v| v.as_bool()),
        Some(false)
    );
    let ret_msg = ret_resp
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(ret_msg.contains("canGetMethodReturnValues"));

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_evaluate_without_frame_id_returns_friendly_message() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();
    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;
    client.attach("127.0.0.1", jdwp.addr().port()).await;

    let eval_resp = client
        .request(
            "evaluate",
            json!({
                "expression": "x",
                "context": "hover",
            }),
        )
        .await;
    assert_eq!(
        eval_resp.get("success").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        eval_resp
            .pointer("/body/variablesReference")
            .and_then(|v| v.as_i64()),
        Some(0)
    );
    let result = eval_resp
        .pointer("/body/result")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(result.contains("frameId"));

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_evaluate_supports_field_and_array_expressions() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();
    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;
    client.attach("127.0.0.1", jdwp.addr().port()).await;

    let thread_id = client.first_thread_id().await;
    let frame_id = client.first_frame_id(thread_id).await;

    let eval_obj_field = client
        .request(
            "evaluate",
            json!({
                "expression": "obj.field",
                "frameId": frame_id,
            }),
        )
        .await;
    assert_eq!(
        eval_obj_field
            .pointer("/body/result")
            .and_then(|v| v.as_str()),
        Some("7")
    );
    assert_eq!(
        eval_obj_field
            .pointer("/body/variablesReference")
            .and_then(|v| v.as_i64()),
        Some(0)
    );

    let eval_arr = client
        .request(
            "evaluate",
            json!({
                "expression": "arr[2]",
                "frameId": frame_id,
            }),
        )
        .await;
    assert_eq!(
        eval_arr.pointer("/body/result").and_then(|v| v.as_str()),
        Some("2")
    );

    let eval_this = client
        .request(
            "evaluate",
            json!({
                "expression": "this.field",
                "frameId": frame_id,
            }),
        )
        .await;
    assert_eq!(
        eval_this.pointer("/body/result").and_then(|v| v.as_str()),
        Some("7")
    );

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_evaluate_supports_pinned_objects_via_nova_pinned_scope() {
    let jdwp = MockJdwpServer::spawn().await.unwrap();
    let (client, server_task) = spawn_wire_server();

    client.initialize_handshake().await;
    client.attach("127.0.0.1", jdwp.addr().port()).await;
    client.set_breakpoints("Main.java", &[3]).await;

    let thread_id = client.first_thread_id().await;
    client.continue_with_thread_id(Some(thread_id)).await;
    let _ = client.wait_for_stopped_reason("breakpoint").await;

    let frame_id = client.first_frame_id(thread_id).await;
    let locals_ref = client.first_scope_variables_reference(frame_id).await;

    let vars_resp = client.variables(locals_ref).await;
    let locals = vars_resp
        .pointer("/body/variables")
        .and_then(|v| v.as_array())
        .unwrap();

    let obj_ref = locals
        .iter()
        .find(|v| v.get("name").and_then(|n| n.as_str()) == Some("obj"))
        .and_then(|v| v.get("variablesReference"))
        .and_then(|v| v.as_i64())
        .expect("expected locals to contain `obj` handle");
    assert!(
        obj_ref > OBJECT_HANDLE_BASE,
        "expected obj to have an object handle"
    );

    let pin_resp = client
        .request(
            "nova/pinObject",
            json!({ "variablesReference": obj_ref, "pinned": true }),
        )
        .await;
    assert_eq!(
        pin_resp.pointer("/body/pinned").and_then(|v| v.as_bool()),
        Some(true)
    );

    let handle_id = obj_ref - OBJECT_HANDLE_BASE;
    assert!(handle_id > 0, "unexpected handle id derived from obj_ref");

    // Fetch the pinned scope (mirrors common DAP client behavior) and ensure child variables use
    // the pinned `evaluateName` base.
    let pinned_vars_resp = client
        .request(
            "variables",
            json!({ "variablesReference": PINNED_SCOPE_REF }),
        )
        .await;
    assert_eq!(
        pinned_vars_resp.get("success").and_then(|v| v.as_bool()),
        Some(true)
    );

    let obj_vars = client
        .request("variables", json!({ "variablesReference": obj_ref }))
        .await;
    let fields = obj_vars
        .pointer("/body/variables")
        .and_then(|v| v.as_array())
        .unwrap();
    let field_eval = fields
        .iter()
        .find(|v| v.get("name").and_then(|n| n.as_str()) == Some("field"))
        .and_then(|v| v.get("evaluateName"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert_eq!(
        field_eval,
        format!("__novaPinned[{handle_id}].field"),
        "expected pinned field evaluateName to use __novaPinned base: {obj_vars}"
    );

    let eval_pinned = client
        .request(
            "evaluate",
            json!({
                "expression": format!("__novaPinned[{handle_id}]"),
                "frameId": frame_id,
            }),
        )
        .await;
    assert_eq!(
        eval_pinned.get("success").and_then(|v| v.as_bool()),
        Some(true),
        "evaluate response was not successful: {eval_pinned}"
    );
    assert_eq!(
        eval_pinned
            .pointer("/body/variablesReference")
            .and_then(|v| v.as_i64()),
        Some(obj_ref),
        "expected pinned eval to preserve object handle variablesReference: {eval_pinned}"
    );
    let result = eval_pinned
        .pointer("/body/result")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(!result.is_empty(), "expected non-empty evaluate result");

    let eval_field = client
        .request(
            "evaluate",
            json!({
                "expression": field_eval,
                "frameId": frame_id,
            }),
        )
        .await;
    assert_eq!(
        eval_field.get("success").and_then(|v| v.as_bool()),
        Some(true),
        "evaluate response was not successful: {eval_field}"
    );
    assert_eq!(
        eval_field.pointer("/body/result").and_then(|v| v.as_str()),
        Some("7"),
        "expected pinned field eval to return 7: {eval_field}"
    );
    assert_eq!(
        eval_field
            .pointer("/body/variablesReference")
            .and_then(|v| v.as_i64()),
        Some(0),
        "expected pinned field eval to return a primitive value: {eval_field}"
    );

    // Unknown handles should return a friendly result string rather than failing the request.
    let eval_unknown = client
        .request(
            "evaluate",
            json!({
                "expression": "__novaPinned[9999999].field",
                "frameId": frame_id,
            }),
        )
        .await;
    assert_eq!(
        eval_unknown.get("success").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        eval_unknown
            .pointer("/body/variablesReference")
            .and_then(|v| v.as_i64()),
        Some(0)
    );
    let unknown_result = eval_unknown
        .pointer("/body/result")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        unknown_result.contains("pinned object"),
        "unexpected response: {eval_unknown}"
    );

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_emits_output_for_expression_value_on_step() {
    // Enable `canGetMethodReturnValues` so the adapter can install a MethodExitWithReturnValue
    // request during stepping.
    let mut caps = vec![false; 32];
    caps[22] = true;
    let jdwp = MockJdwpServer::spawn_with_capabilities(caps).await.unwrap();

    let (client, server_task) = spawn_wire_server();
    client.initialize_handshake().await;
    client.attach("127.0.0.1", jdwp.addr().port()).await;

    let thread_id = client.first_thread_id().await;

    let req_seq = client
        .send_request("next", json!({ "threadId": thread_id }))
        .await;
    let resp = client.wait_for_response(req_seq).await;
    assert!(resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

    // Issue a step-over request and expect:
    //  - an `output` event for the expression value
    //  - a `stopped` event (reason=step)
    let output_evt = client
        .wait_for_event_matching("output(Expression value)", Duration::from_secs(5), |msg| {
            msg.get("type").and_then(|v| v.as_str()) == Some("event")
                && msg.get("event").and_then(|v| v.as_str()) == Some("output")
                && msg
                    .pointer("/body/output")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .contains("Expression value:")
        })
        .await;

    let stopped_evt = client
        .wait_for_event_matching("stopped(step)", Duration::from_secs(5), |msg| {
            msg.get("type").and_then(|v| v.as_str()) == Some("event")
                && msg.get("event").and_then(|v| v.as_str()) == Some("stopped")
                && msg.pointer("/body/reason").and_then(|v| v.as_str()) == Some("step")
        })
        .await;

    // The output should be emitted before the stop notification (mirrors legacy UX).
    let output_seq = output_evt.get("seq").and_then(|v| v.as_i64()).unwrap_or(0);
    let stopped_seq = stopped_evt.get("seq").and_then(|v| v.as_i64()).unwrap_or(0);
    assert!(output_seq < stopped_seq);

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}
