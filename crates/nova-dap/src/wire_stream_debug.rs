use nova_jdwp::wire::{
    inspect, JdwpClient, JdwpError, JdwpValue, MethodId, ObjectId, ReferenceTypeId, ThreadId,
};

/// Temporarily pins a JDWP object id (best-effort) by issuing
/// `ObjectReference.DisableCollection` on creation and `EnableCollection` on drop.
///
/// This is primarily intended for *temporary* objects created as a byproduct of
/// evaluation (e.g. stream-debug sampling invoking `collect(toList())`), which
/// can otherwise be garbage collected immediately after the invoked method
/// returns.
///
/// ## Best-effort semantics
/// - If disable/enable fails (including `VmError(ERROR_INVALID_OBJECT)`), the
///   error is ignored.
/// - The guard tries to synchronously re-enable collection in `Drop` so we do
///   not leak pins across requests.
///
/// ## Important
/// Do **not** use this guard for long-lived/persistently pinned objects (e.g.
/// ones pinned via user interaction in the variables UI). JDWP collection
/// enable/disable is not ref-counted; enabling collection here could undo a
/// persistent pin.
#[derive(Clone)]
struct TemporaryObjectPin {
    jdwp: JdwpClient,
    object_id: ObjectId,
}

impl TemporaryObjectPin {
    async fn new(jdwp: &JdwpClient, object_id: ObjectId) -> Self {
        if object_id != 0 {
            // Best-effort: the object may already be invalid/collected.
            let _ = jdwp.object_reference_disable_collection(object_id).await;
        }
        Self {
            jdwp: jdwp.clone(),
            object_id,
        }
    }
}

impl Drop for TemporaryObjectPin {
    fn drop(&mut self) {
        let object_id = self.object_id;
        if object_id == 0 {
            return;
        }

        let jdwp = self.jdwp.clone();
        let Some(handle) = tokio::runtime::Handle::try_current().ok() else {
            // No tokio runtime: best-effort.
            return;
        };

        // Prefer a synchronous (scoped) enable call so pins are always released
        // before returning from the higher-level request handler.
        let enable_res = std::panic::catch_unwind(std::panic::AssertUnwindSafe({
            let handle = handle.clone();
            let jdwp = jdwp.clone();
            move || {
                tokio::task::block_in_place(|| {
                    handle.block_on(async move {
                        let _ = jdwp.object_reference_enable_collection(object_id).await;
                    });
                });
            }
        }));

        if enable_res.is_ok() {
            return;
        }

        // Fallback: if `block_in_place` is unavailable (e.g. current-thread
        // runtime), spawn and detach.
        let _ = handle.spawn(async move {
            let _ = jdwp.object_reference_enable_collection(object_id).await;
        });
    }
}

/// Stream-debug sampling helper.
///
/// Invokes a helper method (via JDWP `ClassType.InvokeMethod`) that returns an
/// object ID, pins the returned object, then reads its children for display.
///
/// This mirrors the stream-debug sampling pattern of producing a fresh
/// `List` (e.g. from `stream.collect(toList())`) and immediately inspecting it.
pub async fn sample_object_children_via_invoke_method(
    jdwp: &JdwpClient,
    class_id: ReferenceTypeId,
    thread: ThreadId,
    method_id: MethodId,
    args: &[JdwpValue],
) -> Result<Vec<(String, JdwpValue, Option<String>)>, JdwpError> {
    let (value, exception) = jdwp
        .class_type_invoke_method(class_id, thread, method_id, args, 0)
        .await?;
    if exception != 0 {
        return Err(JdwpError::Protocol(format!(
            "exception thrown during invoke_method (id=0x{exception:x})"
        )));
    }

    let object_id = match value {
        JdwpValue::Object { id, .. } => id,
        _ => return Ok(Vec::new()),
    };
    if object_id == 0 {
        return Ok(Vec::new());
    }

    let _pin = TemporaryObjectPin::new(jdwp, object_id).await;
    inspect::object_children(jdwp, object_id).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use nova_jdwp::wire::mock::{DelayedReply, MockJdwpServer, MockJdwpServerConfig};

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sample_stream_inspection_temporarily_pins_object_ids() {
        let mut config = MockJdwpServerConfig::default();
        // Delay a command that runs *after* we pin but *before* we return, so
        // the test can observe the pinned set mid-flight.
        config.delayed_replies = vec![DelayedReply {
            command_set: 9, // ObjectReference
            command: 2,     // GetValues
            delay: Duration::from_millis(200),
        }];

        let server = MockJdwpServer::spawn_with_config(config).await.unwrap();
        let client = JdwpClient::connect(server.addr()).await.unwrap();

        let thread = client.all_threads().await.unwrap()[0];
        let object_id: ObjectId = 0xDEAD_BEEF;

        assert!(server.pinned_object_ids().await.is_empty());

        let client_task = client.clone();
        let task = tokio::spawn(async move {
            let arg = JdwpValue::Object {
                tag: b'L',
                id: object_id,
            };
            sample_object_children_via_invoke_method(&client_task, 0x9001, thread, 0x4001, &[arg])
                .await
                .unwrap()
        });

        // Wait until the sampling code has pinned the object.
        for _ in 0..50 {
            if server.pinned_object_ids().await.contains(&object_id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        assert!(
            server.pinned_object_ids().await.contains(&object_id),
            "expected object to be pinned during inspection"
        );

        let children = task.await.unwrap();
        assert!(
            !children.is_empty(),
            "expected mock object to have at least one child"
        );

        assert!(
            server.pinned_object_ids().await.is_empty(),
            "expected pin to be released after sampling"
        );
    }
}
