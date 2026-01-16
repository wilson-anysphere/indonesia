use crate::rpc_out::RpcOut;
use crate::ServerState;

pub(super) fn flush_memory_status_notifications(
    out: &impl RpcOut,
    state: &mut ServerState,
) -> std::io::Result<()> {
    let mut events = state.memory_events.lock().unwrap();
    if events.is_empty() {
        return Ok(());
    }

    // Avoid spamming: publish only the latest state.
    let last = events.pop().expect("checked non-empty");
    events.clear();
    drop(events);

    let mut top_components = state.memory.report_detailed().1;
    top_components.truncate(10);
    let params = nova_lsp::memory_status_response_value(last.report, top_components)
        .unwrap_or(serde_json::Value::Null);
    out.send_notification(nova_lsp::MEMORY_STATUS_NOTIFICATION, params)?;
    Ok(())
}

pub(super) fn flush_safe_mode_notifications(
    out: &impl RpcOut,
    state: &mut ServerState,
) -> std::io::Result<()> {
    let (enabled, reason) = nova_lsp::hardening::safe_mode_snapshot();
    if enabled == state.last_safe_mode_enabled && reason == state.last_safe_mode_reason {
        return Ok(());
    }

    if enabled && !state.last_safe_mode_enabled {
        state.cancel_semantic_search_workspace_indexing();
    }

    state.last_safe_mode_enabled = enabled;
    state.last_safe_mode_reason = reason;

    let params =
        nova_lsp::safe_mode_status_response_value(enabled, reason.map(ToString::to_string));
    out.send_notification(nova_lsp::SAFE_MODE_CHANGED_NOTIFICATION, params)?;
    Ok(())
}
