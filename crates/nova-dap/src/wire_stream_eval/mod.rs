pub mod java_gen;
pub mod java_types;
pub mod javac_config;
pub mod bindings;

use nova_jdwp::wire::{JdwpClient, JdwpError, JdwpValue, ObjectId, ThreadId};

/// Define a class in the target VM, resolve a `stage0` method, and invoke it.
///
/// This is the JDWP-only portion of Nova's compile+inject evaluator used by stream-debug.
/// It is factored out so it can be exercised in CI using `MockJdwpServer` without requiring
/// a local JDK (no `javac`/`java`).
pub async fn define_class_and_invoke_stage0(
    jdwp: &JdwpClient,
    loader: ObjectId,
    thread: ThreadId,
    class_name: &str,
    bytecode: &[u8],
    args: &[JdwpValue],
) -> Result<JdwpValue, JdwpError> {
    let class_id = jdwp
        .class_loader_define_class(loader, class_name, bytecode)
        .await?;

    let methods = jdwp.reference_type_methods(class_id).await?;
    let stage0 = methods.iter().find(|m| m.name == "stage0").ok_or_else(|| {
        JdwpError::Protocol(format!(
            "expected injected class {class_id} to expose a stage0 method"
        ))
    })?;

    let (value, exception) = jdwp
        .class_type_invoke_method(class_id, thread, stage0.method_id, args, 0)
        .await?;
    if exception != 0 {
        return Err(JdwpError::Protocol(format!(
            "stage0 invocation failed with exception object id {exception}"
        )));
    }

    Ok(value)
}
